//! HTTP Request message related code.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::{cmp, fmt, io, str};

use bytes::Bytes;
use cookie::Cookie;
use failure;
use futures::{Async, Poll, Stream};
use futures_cpupool::CpuPool;
use http::{header, HeaderMap, Method, StatusCode, Uri, Version};
use tokio_io::AsyncRead;
use url::{form_urlencoded, Url};

use body::Body;
use error::{CookieParseError, PayloadError, UrlGenerationError};
use extensions::Extensions;
use handler::FromRequest;
use httpmessage::HttpMessage;
use httpresponse::{HttpResponse, HttpResponseBuilder};
use info::ConnectionInfo;
use param::Params;
use payload::Payload;
use router::{Resource, Router};
use server::helpers::SharedHttpInnerMessage;
use server::message::{HttpRequestContext, MessageFlags};
use uri::Url as InnerUrl;

struct Query(HashMap<String, String>);
struct Cookies(Vec<Cookie<'static>>);
struct Info(ConnectionInfo);

#[derive(Debug, Copy, Clone, PartialEq)]
enum RouterResource {
    Notset,
    Normal(u16),
}

/// An HTTP Request
pub struct HttpRequest<S = ()> {
    msg: SharedHttpInnerMessage,
    state: Option<Rc<S>>,
    router: Option<Router>,
    resource: RouterResource,
}

impl HttpRequest<()> {
    /// Construct a new Request.
    #[inline]
    pub(crate) fn new(
        method: Method, uri: Uri, version: Version, headers: HeaderMap,
        payload: Option<Payload>,
    ) -> HttpRequest {
        let url = InnerUrl::new(uri);
        HttpRequest {
            msg: SharedHttpInnerMessage::from_message(HttpRequestContext {
                method,
                url,
                version,
                headers,
                payload,
                params: Params::new(),
                extensions: Extensions::new(),
                addr: None,
                prefix: 0,
                flags: MessageFlags::empty(),
            }),
            state: None,
            router: None,
            resource: RouterResource::Notset,
        }
    }

    #[inline(always)]
    #[cfg_attr(feature = "cargo-clippy", allow(inline_always))]
    pub(crate) fn from_message(msg: SharedHttpInnerMessage) -> HttpRequest {
        HttpRequest {
            msg,
            state: None,
            router: None,
            resource: RouterResource::Notset,
        }
    }

    #[inline]
    /// Construct new http request with state.
    pub fn with_state<S>(self, state: Rc<S>, router: Router) -> HttpRequest<S> {
        HttpRequest {
            msg: self.msg,
            state: Some(state),
            router: Some(router),
            resource: RouterResource::Notset,
        }
    }

    pub(crate) fn clone_with_state<S>(
        &self, state: Rc<S>, router: Router,
    ) -> HttpRequest<S> {
        HttpRequest {
            msg: self.msg.clone(),
            state: Some(state),
            router: Some(router),
            resource: self.resource,
        }
    }
}

impl<S> HttpMessage for HttpRequest<S> {
    #[inline]
    fn headers(&self) -> &HeaderMap {
        &self.as_ref().headers
    }
}

impl<S> HttpRequest<S> {
    #[inline]
    /// Construct new http request with state.
    pub fn change_state<NS>(&self, state: Rc<NS>) -> HttpRequest<NS> {
        HttpRequest {
            msg: self.msg.clone(),
            state: Some(state),
            router: self.router.clone(),
            resource: self.resource,
        }
    }

    #[inline]
    /// Construct new http request without state.
    pub fn drop_state(&self) -> HttpRequest {
        HttpRequest {
            msg: self.msg.clone(),
            state: None,
            router: self.router.clone(),
            resource: self.resource,
        }
    }

    /// get mutable reference for inner message
    /// mutable reference should not be returned as result for request's method
    #[inline]
    pub(crate) fn as_mut(&mut self) -> &mut HttpRequestContext {
        self.msg.get_mut()
    }

    #[inline]
    fn as_ref(&self) -> &HttpRequestContext {
        self.msg.get_ref()
    }

    /// Shared application state
    #[inline]
    pub fn state(&self) -> &S {
        self.state.as_ref().unwrap()
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> &Extensions {
        self.as_ref().extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        self.as_mut().extensions_mut()
    }

    /// Default `CpuPool`
    #[inline]
    #[doc(hidden)]
    pub fn cpu_pool(&self) -> &CpuPool {
        self.router()
            .expect("HttpRequest has to have Router instance")
            .server_settings()
            .cpu_pool()
    }

    /// Create http response
    pub fn response(&self, status: StatusCode, body: Body) -> HttpResponse {
        if let Some(router) = self.router() {
            router.server_settings().get_response(status, body)
        } else {
            HttpResponse::with_body(status, body)
        }
    }

    /// Create http response builder
    pub fn build_response(&self, status: StatusCode) -> HttpResponseBuilder {
        if let Some(router) = self.router() {
            router.server_settings().get_response_builder(status)
        } else {
            HttpResponse::build(status)
        }
    }

    #[doc(hidden)]
    pub fn prefix_len(&self) -> u16 {
        self.as_ref().prefix as u16
    }

    #[doc(hidden)]
    pub fn set_prefix_len(&mut self, len: u16) {
        self.as_mut().prefix = len;
    }

    /// Read the Request Uri.
    #[inline]
    pub fn uri(&self) -> &Uri {
        self.as_ref().url.uri()
    }

    /// Read the Request method.
    #[inline]
    pub fn method(&self) -> &Method {
        &self.as_ref().method
    }

    /// Read the Request Version.
    #[inline]
    pub fn version(&self) -> Version {
        self.as_ref().version
    }

    ///Returns mutable Request's headers.
    ///
    ///This is intended to be used by middleware.
    #[cfg(test)]
    pub(crate) fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.as_mut().headers
    }

    /// The target path of this Request.
    #[inline]
    pub fn path(&self) -> &str {
        self.as_ref().url.path()
    }

    #[inline]
    pub(crate) fn url(&self) -> &InnerUrl {
        &self.as_ref().url
    }

    /// Get *ConnectionInfo* for correct request.
    pub fn connection_info(&self) -> &ConnectionInfo {
        if self.extensions().get::<Info>().is_none() {
            let mut req = self.clone();
            req.as_mut()
                .extensions
                .insert(Info(ConnectionInfo::new(self)));
        }
        &self.extensions().get::<Info>().unwrap().0
    }

    /// Generate url for named resource
    ///
    /// ```rust
    /// # extern crate actix_web;
    /// # use actix_web::{App, HttpRequest, HttpResponse, http};
    /// #
    /// fn index(req: HttpRequest) -> HttpResponse {
    ///     let url = req.url_for("foo", &["1", "2", "3"]); // <- generate url for "foo" resource
    ///     HttpResponse::Ok().into()
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .resource("/test/{one}/{two}/{three}", |r| {
    ///              r.name("foo");  // <- set resource name, then it could be used in `url_for`
    ///              r.method(http::Method::GET).f(|_| HttpResponse::Ok());
    ///         })
    ///         .finish();
    /// }
    /// ```
    pub fn url_for<U, I>(
        &self, name: &str, elements: U,
    ) -> Result<Url, UrlGenerationError>
    where
        U: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        if self.router().is_none() {
            Err(UrlGenerationError::RouterNotAvailable)
        } else {
            let path = self.router().unwrap().resource_path(name, elements)?;
            if path.starts_with('/') {
                let conn = self.connection_info();
                Ok(Url::parse(&format!(
                    "{}://{}{}",
                    conn.scheme(),
                    conn.host(),
                    path
                ))?)
            } else {
                Ok(Url::parse(&path)?)
            }
        }
    }

    /// Generate url for named resource
    ///
    /// This method is similar to `HttpRequest::url_for()` but it can be used
    /// for urls that do not contain variable parts.
    pub fn url_for_static(&self, name: &str) -> Result<Url, UrlGenerationError> {
        const NO_PARAMS: [&str; 0] = [];
        self.url_for(name, &NO_PARAMS)
    }

    /// This method returns reference to current `Router` object.
    #[inline]
    pub fn router(&self) -> Option<&Router> {
        self.router.as_ref()
    }

    /// This method returns reference to matched `Resource` object.
    #[inline]
    pub fn resource(&self) -> Option<&Resource> {
        if let Some(ref router) = self.router {
            if let RouterResource::Normal(idx) = self.resource {
                return Some(router.get_resource(idx as usize));
            }
        }
        None
    }

    pub(crate) fn set_resource(&mut self, res: usize) {
        self.resource = RouterResource::Normal(res as u16);
    }

    /// Peer socket address
    ///
    /// Peer address is actual socket address, if proxy is used in front of
    /// actix http server, then peer address would be address of this proxy.
    ///
    /// To get client connection information `connection_info()` method should
    /// be used.
    #[inline]
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.as_ref().addr
    }

    #[inline]
    pub(crate) fn set_peer_addr(&mut self, addr: Option<SocketAddr>) {
        self.as_mut().addr = addr;
    }

    /// url query parameters.
    pub fn query(&self) -> &HashMap<String, String> {
        if self.extensions().get::<Query>().is_none() {
            let mut query = HashMap::new();
            for (key, val) in form_urlencoded::parse(self.query_string().as_ref()) {
                query.insert(key.as_ref().to_string(), val.to_string());
            }
            let mut req = self.clone();
            req.as_mut().extensions.insert(Query(query));
        }
        &self.extensions().get::<Query>().unwrap().0
    }

    /// The query string in the URL.
    ///
    /// E.g., id=10
    #[inline]
    pub fn query_string(&self) -> &str {
        if let Some(query) = self.uri().query().as_ref() {
            query
        } else {
            ""
        }
    }

    /// Load request cookies.
    pub fn cookies(&self) -> Result<&Vec<Cookie<'static>>, CookieParseError> {
        if self.extensions().get::<Query>().is_none() {
            let mut req = self.clone();
            let msg = req.as_mut();
            let mut cookies = Vec::new();
            for hdr in msg.headers.get_all(header::COOKIE) {
                let s = str::from_utf8(hdr.as_bytes()).map_err(CookieParseError::from)?;
                for cookie_str in s.split(';').map(|s| s.trim()) {
                    if !cookie_str.is_empty() {
                        cookies.push(Cookie::parse_encoded(cookie_str)?.into_owned());
                    }
                }
            }
            msg.extensions.insert(Cookies(cookies));
        }
        Ok(&self.extensions().get::<Cookies>().unwrap().0)
    }

    /// Return request cookie.
    pub fn cookie(&self, name: &str) -> Option<&Cookie> {
        if let Ok(cookies) = self.cookies() {
            for cookie in cookies {
                if cookie.name() == name {
                    return Some(cookie);
                }
            }
        }
        None
    }

    pub(crate) fn set_cookies(&mut self, cookies: Option<Vec<Cookie<'static>>>) {
        if let Some(cookies) = cookies {
            self.extensions_mut().insert(Cookies(cookies));
        }
    }

    /// Get a reference to the Params object.
    ///
    /// Params is a container for url parameters.
    /// A variable segment is specified in the form `{identifier}`,
    /// where the identifier can be used later in a request handler to
    /// access the matched value for that segment.
    #[inline]
    pub fn match_info(&self) -> &Params {
        &self.as_ref().params
    }

    /// Get mutable reference to request's Params.
    #[inline]
    pub(crate) fn match_info_mut(&mut self) -> &mut Params {
        &mut self.as_mut().params
    }

    /// Checks if a connection should be kept alive.
    pub fn keep_alive(&self) -> bool {
        self.as_ref().flags.contains(MessageFlags::KEEPALIVE)
    }

    /// Check if request requires connection upgrade
    pub(crate) fn upgrade(&self) -> bool {
        if let Some(conn) = self.as_ref().headers.get(header::CONNECTION) {
            if let Ok(s) = conn.to_str() {
                return s.to_lowercase().contains("upgrade");
            }
        }
        self.as_ref().method == Method::CONNECT
    }

    /// Set read buffer capacity
    ///
    /// Default buffer capacity is 32Kb.
    pub fn set_read_buffer_capacity(&mut self, cap: usize) {
        if let Some(ref mut payload) = self.as_mut().payload {
            payload.set_read_buffer_capacity(cap)
        }
    }

    #[cfg(test)]
    pub(crate) fn payload(&mut self) -> &Payload {
        let msg = self.as_mut();
        if msg.payload.is_none() {
            msg.payload = Some(Payload::empty());
        }
        msg.payload.as_ref().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn payload_mut(&mut self) -> &mut Payload {
        let msg = self.as_mut();
        if msg.payload.is_none() {
            msg.payload = Some(Payload::empty());
        }
        msg.payload.as_mut().unwrap()
    }
}

impl Default for HttpRequest<()> {
    /// Construct default request
    fn default() -> HttpRequest {
        HttpRequest {
            msg: SharedHttpInnerMessage::default(),
            state: None,
            router: None,
            resource: RouterResource::Notset,
        }
    }
}

impl<S> Clone for HttpRequest<S> {
    fn clone(&self) -> HttpRequest<S> {
        HttpRequest {
            msg: self.msg.clone(),
            state: self.state.clone(),
            router: self.router.clone(),
            resource: self.resource,
        }
    }
}

impl<S> FromRequest<S> for HttpRequest<S> {
    type Config = ();
    type Result = Self;

    #[inline]
    fn from_request(req: &HttpRequest<S>, _: &Self::Config) -> Self::Result {
        req.clone()
    }
}

impl<S> Stream for HttpRequest<S> {
    type Item = Bytes;
    type Error = PayloadError;

    fn poll(&mut self) -> Poll<Option<Bytes>, PayloadError> {
        let msg = self.as_mut();
        if msg.payload.is_none() {
            Ok(Async::Ready(None))
        } else {
            msg.payload.as_mut().unwrap().poll()
        }
    }
}

impl<S> io::Read for HttpRequest<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.as_mut().payload.is_some() {
            match self.as_mut().payload.as_mut().unwrap().poll() {
                Ok(Async::Ready(Some(mut b))) => {
                    let i = cmp::min(b.len(), buf.len());
                    buf.copy_from_slice(&b.split_to(i)[..i]);

                    if !b.is_empty() {
                        self.as_mut().payload.as_mut().unwrap().unread_data(b);
                    }

                    if i < buf.len() {
                        match self.read(&mut buf[i..]) {
                            Ok(n) => Ok(i + n),
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(i),
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(i)
                    }
                }
                Ok(Async::Ready(None)) => Ok(0),
                Ok(Async::NotReady) => {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "Not ready"))
                }
                Err(e) => Err(io::Error::new(
                    io::ErrorKind::Other,
                    failure::Error::from(e).compat(),
                )),
            }
        } else {
            Ok(0)
        }
    }
}

impl<S> AsyncRead for HttpRequest<S> {}

impl<S> fmt::Debug for HttpRequest<S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let res = writeln!(
            f,
            "\nHttpRequest {:?} {}:{}",
            self.as_ref().version,
            self.as_ref().method,
            self.path()
        );
        if !self.query_string().is_empty() {
            let _ = writeln!(f, "  query: ?{:?}", self.query_string());
        }
        if !self.match_info().is_empty() {
            let _ = writeln!(f, "  params: {:?}", self.as_ref().params);
        }
        let _ = writeln!(f, "  headers:");
        for (key, val) in self.as_ref().headers.iter() {
            let _ = writeln!(f, "    {:?}: {:?}", key, val);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resource::ResourceHandler;
    use router::Resource;
    use server::ServerSettings;
    use test::TestRequest;

    #[test]
    fn test_debug() {
        let req = TestRequest::with_header("content-type", "text/plain").finish();
        let dbg = format!("{:?}", req);
        assert!(dbg.contains("HttpRequest"));
    }

    #[test]
    fn test_no_request_cookies() {
        let req = HttpRequest::default();
        assert!(req.cookies().unwrap().is_empty());
    }

    #[test]
    fn test_request_cookies() {
        let req = TestRequest::default()
            .header(header::COOKIE, "cookie1=value1")
            .header(header::COOKIE, "cookie2=value2")
            .finish();
        {
            let cookies = req.cookies().unwrap();
            assert_eq!(cookies.len(), 2);
            assert_eq!(cookies[0].name(), "cookie1");
            assert_eq!(cookies[0].value(), "value1");
            assert_eq!(cookies[1].name(), "cookie2");
            assert_eq!(cookies[1].value(), "value2");
        }

        let cookie = req.cookie("cookie1");
        assert!(cookie.is_some());
        let cookie = cookie.unwrap();
        assert_eq!(cookie.name(), "cookie1");
        assert_eq!(cookie.value(), "value1");

        let cookie = req.cookie("cookie-unknown");
        assert!(cookie.is_none());
    }

    #[test]
    fn test_request_query() {
        let req = TestRequest::with_uri("/?id=test").finish();
        assert_eq!(req.query_string(), "id=test");
        let query = req.query();
        assert_eq!(&query["id"], "test");
    }

    #[test]
    fn test_request_match_info() {
        let mut req = TestRequest::with_uri("/value/?id=test").finish();

        let mut resource = ResourceHandler::<()>::default();
        resource.name("index");
        let mut routes = Vec::new();
        routes.push((Resource::new("index", "/{key}/"), Some(resource)));
        let (router, _) = Router::new("", ServerSettings::default(), routes);
        assert!(router.recognize(&mut req).is_some());

        assert_eq!(req.match_info().get("key"), Some("value"));
    }

    #[test]
    fn test_url_for() {
        let req2 = HttpRequest::default();
        assert_eq!(
            req2.url_for("unknown", &["test"]),
            Err(UrlGenerationError::RouterNotAvailable)
        );

        let mut resource = ResourceHandler::<()>::default();
        resource.name("index");
        let routes =
            vec![(Resource::new("index", "/user/{name}.{ext}"), Some(resource))];
        let (router, _) = Router::new("/", ServerSettings::default(), routes);
        assert!(router.has_route("/user/test.html"));
        assert!(!router.has_route("/test/unknown"));

        let req = TestRequest::with_header(header::HOST, "www.rust-lang.org")
            .finish_with_router(router);

        assert_eq!(
            req.url_for("unknown", &["test"]),
            Err(UrlGenerationError::ResourceNotFound)
        );
        assert_eq!(
            req.url_for("index", &["test"]),
            Err(UrlGenerationError::NotEnoughElements)
        );
        let url = req.url_for("index", &["test", "html"]);
        assert_eq!(
            url.ok().unwrap().as_str(),
            "http://www.rust-lang.org/user/test.html"
        );
    }

    #[test]
    fn test_url_for_with_prefix() {
        let req = TestRequest::with_header(header::HOST, "www.rust-lang.org").finish();

        let mut resource = ResourceHandler::<()>::default();
        resource.name("index");
        let routes = vec![(Resource::new("index", "/user/{name}.html"), Some(resource))];
        let (router, _) = Router::new("/prefix/", ServerSettings::default(), routes);
        assert!(router.has_route("/user/test.html"));
        assert!(!router.has_route("/prefix/user/test.html"));

        let req = req.with_state(Rc::new(()), router);
        let url = req.url_for("index", &["test"]);
        assert_eq!(
            url.ok().unwrap().as_str(),
            "http://www.rust-lang.org/prefix/user/test.html"
        );
    }

    #[test]
    fn test_url_for_static() {
        let req = TestRequest::with_header(header::HOST, "www.rust-lang.org").finish();

        let mut resource = ResourceHandler::<()>::default();
        resource.name("index");
        let routes = vec![(Resource::new("index", "/index.html"), Some(resource))];
        let (router, _) = Router::new("/prefix/", ServerSettings::default(), routes);
        assert!(router.has_route("/index.html"));
        assert!(!router.has_route("/prefix/index.html"));

        let req = req.with_state(Rc::new(()), router);
        let url = req.url_for_static("index");
        assert_eq!(
            url.ok().unwrap().as_str(),
            "http://www.rust-lang.org/prefix/index.html"
        );
    }

    #[test]
    fn test_url_for_external() {
        let req = HttpRequest::default();

        let mut resource = ResourceHandler::<()>::default();
        resource.name("index");
        let routes = vec![(
            Resource::external("youtube", "https://youtube.com/watch/{video_id}"),
            None,
        )];
        let (router, _) = Router::new::<()>("", ServerSettings::default(), routes);
        assert!(!router.has_route("https://youtube.com/watch/unknown"));

        let req = req.with_state(Rc::new(()), router);
        let url = req.url_for("youtube", &["oHg5SJYRHA0"]);
        assert_eq!(
            url.ok().unwrap().as_str(),
            "https://youtube.com/watch/oHg5SJYRHA0"
        );
    }
}
