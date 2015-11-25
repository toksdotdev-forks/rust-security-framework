use libc::{size_t, c_void};
use core_foundation::array::CFArray;
use core_foundation::base::{TCFType, Boolean};
use core_foundation_sys::base::OSStatus;
#[cfg(any(feature = "OSX_10_8", target_os = "ios"))]
use core_foundation_sys::base::{kCFAllocatorDefault, CFRelease};
use security_framework_sys::base::{errSecSuccess, errSecIO, errSecBadReq};
use security_framework_sys::secure_transport::*;
use std::io;
use std::io::prelude::*;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::slice;
use std::result;

use {cvt, ErrorNew, CipherSuiteInternals, AsInner};
use base::{Result, Error};
use certificate::SecCertificate;
use cipher_suite::CipherSuite;
use identity::SecIdentity;
use trust::SecTrust;

#[derive(Debug, Copy, Clone)]
pub enum ProtocolSide {
    Server,
    Client,
}

#[derive(Debug, Copy, Clone)]
pub enum ConnectionType {
    Stream,
    #[cfg(any(feature = "OSX_10_8", target_os = "ios"))]
    Datagram,
}

#[derive(Debug)]
pub enum HandshakeError<S> {
    Failure(Error),
    ServerAuthCompleted(MidHandshakeSslStream<S>),
    ClientCertRequested(MidHandshakeSslStream<S>),
    WouldBlock(MidHandshakeSslStream<S>),
}

#[derive(Debug)]
pub struct MidHandshakeSslStream<S>(SslStream<S>);

impl<S> MidHandshakeSslStream<S> {
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.get_mut()
    }

    pub fn context(&self) -> &SslContext {
        &self.0.ctx
    }

    pub fn mut_context(&mut self) -> &mut SslContext {
        &mut self.0.ctx
    }

    pub fn handshake(self) -> result::Result<SslStream<S>, HandshakeError<S>> {
        unsafe {
            match SSLHandshake(self.0.ctx.0) {
                errSecSuccess => Ok(self.0),
                errSSLPeerAuthCompleted => Err(HandshakeError::ServerAuthCompleted(self)),
                errSSLClientCertRequested => Err(HandshakeError::ClientCertRequested(self)),
                errSSLWouldBlock => Err(HandshakeError::WouldBlock(self)),
                err => Err(HandshakeError::Failure(Error::new(err))),
            }
        }
    }
}

#[derive(Debug)]
pub enum SessionState {
    Idle,
    Handshake,
    Connected,
    Closed,
    Aborted,
}

impl SessionState {
    fn from_raw(raw: SSLSessionState) -> SessionState {
        match raw {
            kSSLIdle => SessionState::Idle,
            kSSLHandshake => SessionState::Handshake,
            kSSLConnected => SessionState::Connected,
            kSSLClosed => SessionState::Closed,
            kSSLAborted => SessionState::Aborted,
            _ => panic!("bad session state value {}", raw),
        }
    }
}

#[derive(Debug)]
pub enum SslAuthenticate {
    Never,
    Always,
    Try,
}

#[derive(Debug)]
pub enum SslClientCertificateState {
    None,
    Requested,
    Sent,
    Rejected,
}

pub struct SslContext(SSLContextRef);

impl fmt::Debug for SslContext {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = fmt.debug_struct("SslContext");
        if let Ok(state) = self.state() {
            builder.field("state", &state);
        }
        builder.finish()
    }
}

unsafe impl Send for SslContext {}

impl AsInner for SslContext {
    type Inner = SSLContextRef;

    fn as_inner(&self) -> SSLContextRef {
        self.0
    }
}

impl SslContext {
    pub fn new(side: ProtocolSide, type_: ConnectionType) -> Result<SslContext> {
        SslContext::new_inner(side, type_)
    }

    #[cfg(not(any(feature = "OSX_10_8", target_os = "ios")))]
    fn new_inner(side: ProtocolSide, _: ConnectionType) -> Result<SslContext> {
        unsafe {
            let is_server = match side {
                ProtocolSide::Server => 1,
                ProtocolSide::Client => 0,
            };

            let mut ctx = ptr::null_mut();
            try!(cvt(SSLNewContext(is_server, &mut ctx)));
            Ok(SslContext(ctx))
        }
    }

    #[cfg(any(feature = "OSX_10_8", target_os = "ios"))]
    pub fn new_inner(side: ProtocolSide, type_: ConnectionType) -> Result<SslContext> {
        let side = match side {
            ProtocolSide::Server => kSSLServerSide,
            ProtocolSide::Client => kSSLClientSide,
        };

        let type_ = match type_ {
            ConnectionType::Stream => kSSLStreamType,
            ConnectionType::Datagram => kSSLDatagramType,
        };

        unsafe {
            let ctx = SSLCreateContext(kCFAllocatorDefault, side, type_);
            Ok(SslContext(ctx))
        }
    }

    pub fn set_peer_domain_name(&mut self, peer_name: &str) -> Result<()> {
        unsafe {
            // SSLSetPeerDomainName doesn't need a null terminated string
            cvt(SSLSetPeerDomainName(self.0,
                                     peer_name.as_ptr() as *const _,
                                     peer_name.len() as size_t))
        }
    }

    pub fn set_certificate(&mut self,
                           identity: &SecIdentity,
                           certs: &[SecCertificate])
                           -> Result<()> {
        let mut arr = vec![identity.as_CFType()];
        arr.extend(certs.iter().map(|c| c.as_CFType()));
        let certs = CFArray::from_CFTypes(&arr);

        unsafe { cvt(SSLSetCertificate(self.0, certs.as_concrete_TypeRef())) }
    }

    pub fn peer_id(&self) -> Result<Option<&[u8]>> {
        unsafe {
            let mut ptr = ptr::null();
            let mut len = 0;
            try!(cvt(SSLGetPeerID(self.0, &mut ptr, &mut len)));
            if ptr.is_null() {
                Ok(None)
            } else {
                Ok(Some(slice::from_raw_parts(ptr as *const _, len)))
            }
        }
    }

    pub fn set_peer_id(&mut self, peer_id: &[u8]) -> Result<()> {
        unsafe {
            cvt(SSLSetPeerID(self.0, peer_id.as_ptr() as *const _, peer_id.len()))
        }
    }

    pub fn supported_ciphers(&self) -> Result<Vec<CipherSuite>> {
        unsafe {
            let mut num_ciphers = 0;
            try!(cvt(SSLGetNumberSupportedCiphers(self.0, &mut num_ciphers)));
            let mut ciphers = Vec::with_capacity(num_ciphers as usize);
            ciphers.set_len(num_ciphers as usize);
            try!(cvt(SSLGetSupportedCiphers(self.0, ciphers.as_mut_ptr(), &mut num_ciphers)));
            Ok(ciphers.iter().map(|c| CipherSuite::from_raw(*c).unwrap()).collect())
        }
    }

    pub fn enabled_ciphers(&self) -> Result<Vec<CipherSuite>> {
        unsafe {
            let mut num_ciphers = 0;
            try!(cvt(SSLGetNumberEnabledCiphers(self.0, &mut num_ciphers)));
            let mut ciphers = Vec::with_capacity(num_ciphers as usize);
            ciphers.set_len(num_ciphers as usize);
            try!(cvt(SSLGetEnabledCiphers(self.0, ciphers.as_mut_ptr(), &mut num_ciphers)));
            Ok(ciphers.iter().map(|c| CipherSuite::from_raw(*c).unwrap()).collect())
        }
    }

    pub fn set_enabled_ciphers(&mut self, ciphers: &[CipherSuite]) -> Result<()> {
        let ciphers = ciphers.iter().map(|c| c.to_raw()).collect::<Vec<_>>();
        unsafe { cvt(SSLSetEnabledCiphers(self.0, ciphers.as_ptr(), ciphers.len() as size_t)) }
    }

    pub fn negotiated_cipher(&self) -> Result<CipherSuite> {
        unsafe {
            let mut cipher = 0;
            try!(cvt(SSLGetNegotiatedCipher(self.0, &mut cipher)));
            Ok(CipherSuite::from_raw(cipher).unwrap())
        }
    }

    pub fn set_client_side_authenticate(&mut self, auth: SslAuthenticate) -> Result<()> {
        let auth = match auth {
            SslAuthenticate::Never => kNeverAuthenticate,
            SslAuthenticate::Always => kAlwaysAuthenticate,
            SslAuthenticate::Try => kTryAuthenticate,
        };

        unsafe {
            cvt(SSLSetClientSideAuthenticate(self.0, auth))
        }
    }

    pub fn client_certificate_state(&self) -> Result<SslClientCertificateState> {
        let mut state = 0;

        unsafe {
            try!(cvt(SSLGetClientCertificateState(self.0, &mut state)));
        }

        let state = match state {
            kSSLClientCertNone => SslClientCertificateState::None,
            kSSLClientCertRequested => SslClientCertificateState::Requested,
            kSSLClientCertSent => SslClientCertificateState::Sent,
            kSSLClientCertRejected => SslClientCertificateState::Rejected,
            _ => panic!("got invalid client cert state {}", state),
        };
        Ok(state)
    }

    pub fn peer_trust(&self) -> Result<SecTrust> {
        // Calling SSLCopyPeerTrust on an idle connection does not seem to be well defined,
        // so explicitly check for that
        if let SessionState::Idle = try!(self.state()) {
            return Err(Error::new(errSecBadReq));
        }

        unsafe {
            let mut trust = ptr::null_mut();
            try!(cvt(SSLCopyPeerTrust(self.0, &mut trust)));
            Ok(SecTrust::wrap_under_create_rule(trust))
        }
    }

    pub fn state(&self) -> Result<SessionState> {
        unsafe {
            let mut state = 0;
            try!(cvt(SSLGetSessionState(self.0, &mut state)));
            Ok(SessionState::from_raw(state))
        }
    }

    pub fn buffered_read_size(&self) -> Result<usize> {
        unsafe {
            let mut size = 0;
            try!(cvt(SSLGetBufferedReadSize(self.0, &mut size)));
            Ok(size)
        }
    }

    pub fn handshake<S>(self, stream: S) -> result::Result<SslStream<S>, HandshakeError<S>>
        where S: Read + Write
    {
        unsafe {
            let ret = SSLSetIOFuncs(self.0, read_func::<S>, write_func::<S>);
            if ret != errSecSuccess {
                return Err(HandshakeError::Failure(Error::new(ret)));
            }

            let stream = Connection {
                stream: stream,
                err: None,
            };
            let stream = Box::into_raw(Box::new(stream)) as *mut _;
            let ret = SSLSetConnection(self.0, stream);
            if ret != errSecSuccess {
                let _conn = Box::<Connection<S>>::from_raw(stream as *mut _);
                return Err(HandshakeError::Failure(Error::new(ret)));
            }

            let stream = SslStream {
                ctx: self,
                _m: PhantomData,
            };

            match SSLHandshake(stream.ctx.0) {
                errSecSuccess => Ok(stream),
                errSSLPeerAuthCompleted => {
                    Err(HandshakeError::ServerAuthCompleted(MidHandshakeSslStream(stream)))
                }
                errSSLClientCertRequested => {
                    Err(HandshakeError::ClientCertRequested(MidHandshakeSslStream(stream)))
                }
                errSSLWouldBlock => Err(HandshakeError::WouldBlock(MidHandshakeSslStream(stream))),
                err => Err(HandshakeError::Failure(Error::new(err))),
            }
        }
    }
}

macro_rules! impl_options {
    ($($(#[$a:meta])* const $opt:ident: $get:ident & $set:ident,)*) => {
        impl SslContext {
            $(
                $(#[$a])*
                pub fn $set(&mut self, value: bool) -> Result<()> {
                    unsafe { cvt(SSLSetSessionOption(self.0, $opt, value as Boolean)) }
                }

                $(#[$a])*
                pub fn $get(&self) -> Result<bool> {
                    let mut value = 0;
                    unsafe { try!(cvt(SSLGetSessionOption(self.0, $opt, &mut value))); }
                    Ok(value != 0)
                }
            )*
        }
    }
}

impl_options! {
    const kSSLSessionOptionBreakOnServerAuth: break_on_server_auth & set_break_on_server_auth,
    const kSSLSessionOptionBreakOnCertRequested: break_on_cert_requested & set_break_on_cert_requested,
    #[cfg(any(feature = "OSX_10_8", target_os = "ios"))]
    const kSSLSessionOptionBreakOnClientAuth: break_on_client_auth & set_break_on_client_auth,
    #[cfg(any(feature = "OSX_10_9", target_os = "ios"))]
    const kSSLSessionOptionFalseStart: false_start & set_false_start,
    #[cfg(any(feature = "OSX_10_9", target_os = "ios"))]
    const kSSLSessionOptionSendOneByteRecord: send_one_byte_record & set_send_one_byte_record,
}

impl Drop for SslContext {
    #[cfg(not(any(feature = "OSX_10_8", target_os = "ios")))]
    fn drop(&mut self) {
        unsafe {
            SSLDisposeContext(self.0);
        }
    }

    #[cfg(any(feature = "OSX_10_8", target_os = "ios"))]
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.as_CFTypeRef());
        }
    }
}

#[cfg(any(feature = "OSX_10_8", target_os = "ios"))]
impl_TCFType!(SslContext, SSLContextRef, SSLContextGetTypeID);

struct Connection<S> {
    stream: S,
    err: Option<io::Error>,
}

// the logic here is based off of libcurl's

fn translate_err(e: &io::Error) -> OSStatus {
    match e.kind() {
        io::ErrorKind::NotFound => errSSLClosedGraceful,
        io::ErrorKind::ConnectionReset => errSSLClosedAbort,
        io::ErrorKind::WouldBlock => errSSLWouldBlock,
        _ => errSecIO,
    }
}

unsafe extern "C" fn read_func<S: Read>(connection: SSLConnectionRef,
                                        data: *mut c_void,
                                        data_length: *mut size_t)
                                        -> OSStatus {
    let mut conn: &mut Connection<S> = mem::transmute(connection);
    let mut data = slice::from_raw_parts_mut(data as *mut u8, *data_length);
    let mut start = 0;
    let mut ret = 0;

    while start < data.len() {
        match conn.stream.read(&mut data[start..]) {
            Ok(0) => {
                ret = errSSLClosedNoNotify;
                break;
            }
            Ok(len) => start += len,
            Err(e) => {
                ret = translate_err(&e);
                conn.err = Some(e);
                break;
            }
        }
    }

    *data_length = start as size_t;
    ret
}

unsafe extern "C" fn write_func<S: Write>(connection: SSLConnectionRef,
                                          data: *const c_void,
                                          data_length: *mut size_t)
                                          -> OSStatus {
    let mut conn: &mut Connection<S> = mem::transmute(connection);
    let data = slice::from_raw_parts(data as *mut u8, *data_length);
    let mut start = 0;
    let mut ret = 0;

    while start < data.len() {
        match conn.stream.write(&data[start..]) {
            Ok(0) => {
                ret = errSSLClosedNoNotify;
                break;
            }
            Ok(len) => start += len,
            Err(e) => {
                ret = translate_err(&e);
                conn.err = Some(e);
                break;
            }
        }
    }

    *data_length = start as size_t;
    ret
}

pub struct SslStream<S> {
    ctx: SslContext,
    _m: PhantomData<S>,
}

impl<S: fmt::Debug> fmt::Debug for SslStream<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("SslStream")
           .field("context", &self.ctx)
           .field("stream", self.get_ref())
           .finish()
    }
}

impl<S> Drop for SslStream<S> {
    fn drop(&mut self) {
        unsafe {
            SSLClose(self.ctx.0);

            let mut conn = ptr::null();
            let ret = SSLGetConnection(self.ctx.0, &mut conn);
            assert!(ret == errSecSuccess);
            Box::<Connection<S>>::from_raw(conn as *mut _);
        }
    }
}

impl<S> SslStream<S> {
    pub fn get_ref(&self) -> &S {
        &self.connection().stream
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.connection_mut().stream
    }

    pub fn context(&self) -> &SslContext {
        &self.ctx
    }

    fn connection(&self) -> &Connection<S> {
        unsafe {
            let mut conn = ptr::null();
            let ret = SSLGetConnection(self.ctx.0, &mut conn);
            assert!(ret == errSecSuccess);

            mem::transmute(conn)
        }
    }

    fn connection_mut(&mut self) -> &mut Connection<S> {
        unsafe {
            let mut conn = ptr::null();
            let ret = SSLGetConnection(self.ctx.0, &mut conn);
            assert!(ret == errSecSuccess);

            mem::transmute(conn)
        }
    }

    fn get_error(&mut self, ret: OSStatus) -> io::Error {
        let conn = self.connection_mut();
        if let Some(err) = conn.err.take() {
            err
        } else {
            io::Error::new(io::ErrorKind::Other, Error::new(ret))
        }
    }
}

impl<S: Read + Write> Read for SslStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let mut nread = 0;
            let ret = SSLRead(self.ctx.0,
                              buf.as_mut_ptr() as *mut _,
                              buf.len() as size_t,
                              &mut nread);
            match ret {
                errSecSuccess => Ok(nread as usize),
                errSSLClosedGraceful |
                errSSLClosedAbort |
                errSSLClosedNoNotify => Ok(0),
                _ => Err(self.get_error(ret)),
            }
        }
    }
}

impl<S: Read + Write> Write for SslStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let mut nwritten = 0;
            let ret = SSLWrite(self.ctx.0,
                               buf.as_ptr() as *const _,
                               buf.len() as size_t,
                               &mut nwritten);
            if ret == errSecSuccess {
                Ok(nwritten as usize)
            } else {
                Err(self.get_error(ret))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.connection_mut().stream.flush()
    }
}

#[cfg(test)]
mod test {
    use std::io::prelude::*;
    use std::net::TcpStream;

    use super::*;

    #[test]
    fn connect() {
        let mut ctx = p!(SslContext::new(ProtocolSide::Client, ConnectionType::Stream));
        p!(ctx.set_peer_domain_name("google.com"));
        let stream = p!(TcpStream::connect("google.com:443"));
        p!(ctx.handshake(stream));
    }

    #[test]
    fn connect_bad_domain() {
        let mut ctx = p!(SslContext::new(ProtocolSide::Client, ConnectionType::Stream));
        p!(ctx.set_peer_domain_name("foobar.com"));
        let stream = p!(TcpStream::connect("google.com:443"));
        match ctx.handshake(stream) {
            Ok(_) => panic!("expected failure"),
            Err(_) => {}
        }
    }

    #[test]
    fn load_page() {
        let mut ctx = p!(SslContext::new(ProtocolSide::Client, ConnectionType::Stream));
        p!(ctx.set_peer_domain_name("google.com"));
        let stream = p!(TcpStream::connect("google.com:443"));
        let mut stream = p!(ctx.handshake(stream));
        p!(stream.write_all(b"GET / HTTP/1.0\r\n\r\n"));
        p!(stream.flush());
        let mut buf = String::new();
        p!(stream.read_to_string(&mut buf));
        assert!(buf.starts_with("HTTP/1.0 200 OK"));
        assert!(buf.ends_with("</html>"));
    }

    #[test]
    fn cipher_configuration() {
        let mut ctx = p!(SslContext::new(ProtocolSide::Server, ConnectionType::Stream));
        let ciphers = p!(ctx.enabled_ciphers());
        let ciphers = ciphers.iter()
                             .enumerate()
                             .filter_map(|(i, c)| {
                                 if i % 2 == 0 {
                                     Some(*c)
                                 } else {
                                     None
                                 }
                             })
                             .collect::<Vec<_>>();
        p!(ctx.set_enabled_ciphers(&ciphers));
        assert_eq!(ciphers, p!(ctx.enabled_ciphers()));
    }

    #[test]
    fn idle_context_peer_trust() {
        let ctx = p!(SslContext::new(ProtocolSide::Server, ConnectionType::Stream));
        assert!(ctx.peer_trust().is_err());
    }

    #[test]
    fn peer_id() {
        let mut ctx = p!(SslContext::new(ProtocolSide::Server, ConnectionType::Stream));
        assert!(p!(ctx.peer_id()).is_none());
        p!(ctx.set_peer_id(b"foobar"));
        assert_eq!(p!(ctx.peer_id()), Some(&b"foobar"[..]));
    }
}
