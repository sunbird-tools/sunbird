use std::{
   ffi::c_void,
   io::{
      Read,
      Write,
   },
   net::{
      SocketAddr,
      TcpListener,
      TcpStream,
   },
   ptr,
   thread,
   time::Duration,
};

use super::socks;

const NO_PROXY: u32 = 1;
const NAMED_PROXY: u32 = 3;
const SECURE: u32 = 0x0080_0000;
const ADD_REPLACE: u32 = 0x2000_0000 | 0x8000_0000;
const QUERY_STATUS: u32 = 0x0013 | 0x2000_0000;
const HTTP: i32 = 1;
const HTTPS: i32 = 2;
const BODY_LIMIT: usize = 2 << 20;

struct Handle(*mut c_void);

impl Drop for Handle {
   fn drop(&mut self) {
      if !self.0.is_null() {
         // SAFETY: the handle came from WinHTTP and has not been closed.
         unsafe { WinHttpCloseHandle(self.0) };
      }
   }
}

#[repr(C)]
struct UrlParts {
   struct_size:    u32,
   scheme:         *mut u16,
   scheme_len:     u32,
   scheme_kind:    i32,
   host_name:      *mut u16,
   host_name_len:  u32,
   port:           u16,
   user_name:      *mut u16,
   user_name_len:  u32,
   password:       *mut u16,
   password_len:   u32,
   url_path:       *mut u16,
   url_path_len:   u32,
   extra_info:     *mut u16,
   extra_info_len: u32,
}

#[link(name = "winhttp")]
unsafe extern "system" {
   fn WinHttpOpen(
      agent: *const u16,
      access: u32,
      proxy: *const u16,
      bypass: *const u16,
      flags: u32,
   ) -> *mut c_void;
   fn WinHttpCloseHandle(handle: *mut c_void) -> i32;
   fn WinHttpSetTimeouts(handle: *mut c_void, resolve: i32, connect: i32, send: i32, receive: i32)
   -> i32;
   fn WinHttpCrackUrl(url: *const u16, url_len: u32, flags: u32, parts: *mut UrlParts) -> i32;
   fn WinHttpConnect(session: *mut c_void, host: *const u16, port: u16, reserved: u32)
   -> *mut c_void;
   fn WinHttpOpenRequest(
      connect: *mut c_void,
      verb: *const u16,
      path: *const u16,
      version: *const u16,
      referrer: *const u16,
      accept: *const *const u16,
      flags: u32,
   ) -> *mut c_void;
   fn WinHttpAddRequestHeaders(request: *mut c_void, headers: *const u16, len: u32, mods: u32)
   -> i32;
   fn WinHttpSendRequest(
      request: *mut c_void,
      headers: *const u16,
      headers_len: u32,
      body: *const c_void,
      body_len: u32,
      total: u32,
      context: usize,
   ) -> i32;
   fn WinHttpReceiveResponse(request: *mut c_void, reserved: *mut c_void) -> i32;
   fn WinHttpQueryHeaders(
      request: *mut c_void,
      info: u32,
      name: *const u16,
      buffer: *mut c_void,
      len: *mut u32,
      index: *mut u32,
   ) -> i32;
   fn WinHttpQueryDataAvailable(request: *mut c_void, available: *mut u32) -> i32;
   fn WinHttpReadData(request: *mut c_void, buffer: *mut c_void, len: u32, read: *mut u32) -> i32;
}

fn wide(text: &str) -> Vec<u16> {
   text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn get(url: &str, timeout_ms: i32) -> Option<Vec<u8>> {
   get_proxied(url, None, timeout_ms)
}

pub fn get_via_socks(url: &str, socks_addr: &str, timeout_ms: i32) -> Option<Vec<u8>> {
   let remote: SocketAddr = socks_addr.parse().ok()?;
   let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).ok()?;
   listener.set_nonblocking(true).ok()?;
   let port = listener.local_addr().ok()?.port();
   let socks = remote.to_string();
   let budget = Duration::from_millis(u64::try_from(timeout_ms.max(0)).unwrap_or(8_000));
   let agent = thread::spawn(move || {
      let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
         serve_connect(&listener, &socks, budget);
      }));
   });
   let body = get_proxied(url, Some(&format!("127.0.0.1:{port}")), timeout_ms);
   let _ = agent.join();
   body
}

fn serve_connect(listener: &TcpListener, socks_addr: &str, budget: Duration) {
   let started = std::time::Instant::now();
   let (mut client, _) = loop {
      match listener.accept() {
         Ok(pair) => break pair,
         Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            if started.elapsed() > budget {
               return;
            }
            thread::sleep(Duration::from_millis(20));
         },
         Err(_) => return,
      }
   };
   let _ = client.set_nonblocking(false);
   let _ = client.set_read_timeout(Some(budget));
   let _ = client.set_write_timeout(Some(budget));
   let mut head = Vec::new();
   let mut buf = [0u8; 512];
   while !head.windows(4).any(|window| window == b"\r\n\r\n") && head.len() < 8192 {
      match client.read(&mut buf) {
         Ok(0) | Err(_) => return,
         Ok(n) => head.extend_from_slice(&buf[..n]),
      }
   }
   let text = String::from_utf8_lossy(&head);
   let Some(line) = text.lines().next() else {
      return;
   };
   let mut parts = line.split_whitespace();
   if parts.next() != Some("CONNECT") {
      return;
   }
   let Some(authority) = parts.next() else {
      return;
   };
   let Some((host, port)) = authority.rsplit_once(':') else {
      return;
   };
   let Ok(port) = port.parse::<u16>() else {
      return;
   };
   let Ok(remote_addr) = socks_addr.parse() else {
      return;
   };
   let Ok(mut remote) = TcpStream::connect_timeout(&remote_addr, budget) else {
      return;
   };
   let _ = remote.set_read_timeout(Some(budget));
   let _ = remote.set_write_timeout(Some(budget));
   let opened = if let Ok(ip) = host.parse() {
      socks::handshake(&mut remote, ip, port)
   } else {
      socks::handshake_host(&mut remote, host, port)
   };
   if !opened {
      return;
   }
   if client
      .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
      .is_err()
   {
      return;
   }
   let _ = splice(client, remote);
}

fn splice(left: TcpStream, right: TcpStream) -> std::io::Result<()> {
   let mut left_read = left.try_clone()?;
   let mut right_write = right.try_clone()?;
   let forward = thread::spawn(move || {
      let _ = std::io::copy(&mut left_read, &mut right_write);
   });
   let mut right_read = right;
   let mut left_write = left;
   let _ = std::io::copy(&mut right_read, &mut left_write);
   let _ = forward.join();
   Ok(())
}

fn get_proxied(url: &str, proxy: Option<&str>, timeout_ms: i32) -> Option<Vec<u8>> {
   let wide_url = wide(url);
   let mut host_buf = vec![0u16; 256];
   let mut path_buf = vec![0u16; wide_url.len().max(256)];
   let mut extra_buf = vec![0u16; wide_url.len().max(256)];
   let mut parts = UrlParts {
      struct_size:    u32::try_from(size_of::<UrlParts>()).unwrap_or(0),
      scheme:         ptr::null_mut(),
      scheme_len:     0,
      scheme_kind:    0,
      host_name:      host_buf.as_mut_ptr(),
      host_name_len:  u32::try_from(host_buf.len()).unwrap_or(0),
      port:           0,
      user_name:      ptr::null_mut(),
      user_name_len:  0,
      password:       ptr::null_mut(),
      password_len:   0,
      url_path:       path_buf.as_mut_ptr(),
      url_path_len:   u32::try_from(path_buf.len()).unwrap_or(0),
      extra_info:     extra_buf.as_mut_ptr(),
      extra_info_len: u32::try_from(extra_buf.len()).unwrap_or(0),
   };
   // SAFETY: `wide_url` is NUL-terminated and the output buffers are writable.
   if unsafe { WinHttpCrackUrl(wide_url.as_ptr(), 0, 0, &raw mut parts) } == 0 {
      return None;
   }
   let secure = match parts.scheme_kind {
      HTTPS => true,
      HTTP => false,
      _ => return None,
   };
   host_buf.truncate(parts.host_name_len as usize);
   host_buf.push(0);
   let mut full_path =
      Vec::with_capacity(parts.url_path_len as usize + parts.extra_info_len as usize + 2);
   if parts.url_path_len == 0 {
      full_path.push(u16::from(b'/'));
   } else {
      full_path.extend_from_slice(&path_buf[..parts.url_path_len as usize]);
   }
   full_path.extend_from_slice(&extra_buf[..parts.extra_info_len as usize]);
   full_path.push(0);

   let agent = wide("Sunbird screen-share (+https://github.com/sunbird-tools/sunbird)");
   let proxy_wide = proxy.map(wide);
   let bypass = [0u16];
   let (access, proxy_ptr, bypass_ptr) = proxy_wide.as_ref().map_or(
      (NO_PROXY, ptr::null(), ptr::null()),
      |proxy| (NAMED_PROXY, proxy.as_ptr(), bypass.as_ptr()),
   );
   // SAFETY: the wide strings outlive the session, which `Handle` closes.
   let session = Handle(unsafe { WinHttpOpen(agent.as_ptr(), access, proxy_ptr, bypass_ptr, 0) });
   if session.0.is_null() {
      return None;
   }
   // SAFETY: `session.0` is a live session.
   unsafe { WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms) };
   // SAFETY: `host_buf` is NUL-terminated.
   let conn = Handle(unsafe { WinHttpConnect(session.0, host_buf.as_ptr(), parts.port, 0) });
   if conn.0.is_null() {
      return None;
   }
   let verb = wide("GET");
   let flags = if secure { SECURE } else { 0 };
   // SAFETY: `verb` and `full_path` are NUL-terminated.
   let req = Handle(unsafe {
      WinHttpOpenRequest(
         conn.0,
         verb.as_ptr(),
         full_path.as_ptr(),
         ptr::null(),
         ptr::null(),
         ptr::null(),
         flags,
      )
   });
   if req.0.is_null() {
      return None;
   }
   let header = wide("Accept: application/json\r\n");
   let header_len = u32::try_from(header.len().saturating_sub(1)).unwrap_or(0);
   // SAFETY: `req.0` is a live request and `header` has `header_len` code units.
   if unsafe { WinHttpAddRequestHeaders(req.0, header.as_ptr(), header_len, ADD_REPLACE) } == 0 {
      return None;
   }
   // SAFETY: the request has no body.
   if unsafe { WinHttpSendRequest(req.0, ptr::null(), 0, ptr::null(), 0, 0, 0) } == 0 {
      return None;
   }
   read_body(req.0)
}

fn read_body(request: *mut c_void) -> Option<Vec<u8>> {
   // SAFETY: `request` has been sent.
   if unsafe { WinHttpReceiveResponse(request, ptr::null_mut()) } == 0 {
      return None;
   }
   let mut status = 0u32;
   let mut status_size = 4u32;
   // SAFETY: `status` is 4 bytes.
   if unsafe {
      WinHttpQueryHeaders(
         request,
         QUERY_STATUS,
         ptr::null(),
         (&raw mut status).cast(),
         &raw mut status_size,
         ptr::null_mut(),
      )
   } == 0
      || status != 200
   {
      return None;
   }
   let mut body = Vec::new();
   loop {
      let mut available = 0u32;
      // SAFETY: `request` has a received response.
      if unsafe { WinHttpQueryDataAvailable(request, &raw mut available) } == 0 {
         return None;
      }
      if available == 0 {
         break;
      }
      let chunk = usize::try_from(available).unwrap_or(0).min(64 << 10);
      if body.len().saturating_add(chunk) > BODY_LIMIT {
         return None;
      }
      let start = body.len();
      body.resize(start + chunk, 0);
      let mut read = 0u32;
      // SAFETY: `body[start..]` has `chunk` writable bytes.
      if unsafe {
         WinHttpReadData(
            request,
            body[start..].as_mut_ptr().cast(),
            u32::try_from(chunk).unwrap_or(0),
            &raw mut read,
         )
      } == 0
      {
         return None;
      }
      body.truncate(start + usize::try_from(read).unwrap_or(0));
      if read == 0 {
         break;
      }
   }
   Some(body)
}
