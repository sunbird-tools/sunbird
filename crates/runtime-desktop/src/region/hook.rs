use std::{
   cell::Cell,
   ffi::{
      c_void,
      CStr,
   },
   mem,
   net::IpAddr,
   ptr,
   sync::atomic::{
      AtomicUsize,
      Ordering,
   },
};

use super::socks::{
   self,
   Bytes,
};

thread_local! {
   static BYPASS: Cell<bool> = const { Cell::new(false) };
}

static CONNECT: AtomicUsize = AtomicUsize::new(0);
static WSA_CONNECT: AtomicUsize = AtomicUsize::new(0);
static IOCTL: AtomicUsize = AtomicUsize::new(0);
static GETADDRINFO: AtomicUsize = AtomicUsize::new(0);
static GETADDRINFOW: AtomicUsize = AtomicUsize::new(0);
static CONNECT_EX: AtomicUsize = AtomicUsize::new(0);

const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;
const SOL_SOCKET: i32 = 0xFFFF;
const SO_TYPE: i32 = 0x1008;
const SO_ERROR: i32 = 0x1007;
const SO_RCVTIMEO: i32 = 0x1006;
const SO_SNDTIMEO: i32 = 0x1005;
const SOCK_STREAM: i32 = 1;
const SD_BOTH: i32 = 2;
const FIONBIO: u32 = 0x8004_667E;
const WSAEWOULDBLOCK: i32 = 10035;
const WSAECONNRESET: i32 = 10054;
const SIO_GET_EXTENSION: u32 = 0xC800_0006;
const SO_UPDATE_CONNECT_CONTEXT: i32 = 0x7010;
const CONNECTEX_GUID: [u8; 16] = [
   0xB9, 0x07, 0xA2, 0x25, 0xF3, 0xDD, 0x60, 0x46, 0x8E, 0xE9, 0x76, 0xE5, 0x8C, 0x74, 0x06, 0x3E,
];

type ConnectFn = unsafe extern "system" fn(usize, *const u8, i32) -> i32;
type WsaConnectFn = unsafe extern "system" fn(
   usize,
   *const u8,
   i32,
   *const u8,
   *mut u8,
   *const u8,
   *const u8,
) -> i32;
type IoctlFn = unsafe extern "system" fn(
   usize,
   u32,
   *const u8,
   u32,
   *mut u8,
   u32,
   *mut u32,
   *mut c_void,
   *mut c_void,
) -> i32;
type GetAddrInfoFn =
   unsafe extern "system" fn(*const u8, *const u8, *const AddrInfo, *mut *mut AddrInfo) -> i32;
type GetAddrInfoWFn =
   unsafe extern "system" fn(*const u16, *const u16, *const AddrInfo, *mut *mut AddrInfo) -> i32;
type ConnectExFn =
   unsafe extern "system" fn(usize, *const u8, i32, *const u8, u32, *mut u32, *mut c_void) -> i32;

#[repr(C)]
struct AddrInfo {
   flags:     i32,
   family:    i32,
   socktype:  i32,
   protocol:  i32,
   addrlen:   usize,
   canonname: *mut u8,
   addr:      *mut u8,
   next:      *mut Self,
}

#[repr(C)]
struct PollFd {
   sock:    usize,
   events:  i16,
   revents: i16,
}

struct Sock(usize);

impl Bytes for Sock {
   fn push(&mut self, data: &[u8]) -> bool {
      send_all(self.0, data)
   }

   fn pull(&mut self, data: &mut [u8]) -> bool {
      recv_exact(self.0, data)
   }
}

#[link(name = "ws2_32")]
unsafe extern "system" {
   fn send(sock: usize, buf: *const u8, len: i32, flags: i32) -> i32;
   fn recv(sock: usize, buf: *mut u8, len: i32, flags: i32) -> i32;
   fn shutdown(sock: usize, how: i32) -> i32;
   fn getsockopt(sock: usize, level: i32, opt: i32, val: *mut u8, len: *mut i32) -> i32;
   fn setsockopt(sock: usize, level: i32, opt: i32, val: *const u8, len: i32) -> i32;
   fn ioctlsocket(sock: usize, cmd: u32, arg: *mut u32) -> i32;
   fn getpeername(sock: usize, addr: *mut u8, len: *mut i32) -> i32;
   fn WSAGetLastError() -> i32;
   fn WSASetLastError(err: i32);
   fn WSAPoll(fds: *mut PollFd, count: u32, timeout: i32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
   fn LoadLibraryW(name: *const u16) -> *mut c_void;
   fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

pub fn install() -> bool {
   let module = load_winsock();
   if module.is_null() {
      log::warn!("screen share: ws2_32 did not load");
      return false;
   }
   let resolver = hook_export(
      module,
      "getaddrinfo",
      fn_addr(getaddrinfo_detour as *const ()),
      &GETADDRINFO,
   ) || hook_export(
      module,
      "GetAddrInfoW",
      fn_addr(getaddrinfow_detour as *const ()),
      &GETADDRINFOW,
   );
   let dial = hook_export(module, "connect", fn_addr(connect_detour as *const ()), &CONNECT)
      || hook_export(
         module,
         "WSAConnect",
         fn_addr(wsa_connect_detour as *const ()),
         &WSA_CONNECT,
      );
   let _ioctl = hook_export(module, "WSAIoctl", fn_addr(ioctl_detour as *const ()), &IOCTL);
   resolver && dial
}

pub fn shutdown_all(sockets: &[usize]) {
   for sock in sockets {
      // SAFETY: the socket was one `connect` returned to Discord, still open
      // until Discord closes it. Shutdown wakes that read.
      unsafe { shutdown(*sock, SD_BOTH) };
   }
}

fn fn_addr(function: *const ()) -> usize {
   function as usize
}

fn load_winsock() -> *mut c_void {
   let name: Vec<u16> = "ws2_32.dll".encode_utf16().chain(std::iter::once(0)).collect();
   // SAFETY: `name` is a NUL-terminated wide string. Called once, before
   // Discord's threads run.
   unsafe { LoadLibraryW(name.as_ptr()) }
}

fn hook_export(module: *mut c_void, name: &str, detour: usize, slot: &AtomicUsize) -> bool {
   let mut symbol = name.as_bytes().to_vec();
   symbol.push(0);
   // SAFETY: `module` is ws2_32 and `symbol` is NUL-terminated.
   let addr = unsafe { GetProcAddress(module, symbol.as_ptr()) };
   if addr.is_null() {
      log::warn!("screen share: {name} is not exported");
      return false;
   }
   // SAFETY: `addr` is the export, `detour` has its ABI, and Discord has not
   // started calling it yet.
   match unsafe { phoenix_hook::image::hook_foreign(addr as usize, detour, slot) } {
      Ok(()) => {
         log::info!("screen share: hooked {name}");
         true
      },
      Err(err) => {
         log::warn!("screen share: {name}: {err}");
         false
      },
   }
}

fn load<F: Copy>(slot: &AtomicUsize) -> Option<F> {
   let ptr = slot.load(Ordering::Acquire);
   if ptr == 0 {
      return None;
   }
   const { assert!(size_of::<F>() == size_of::<usize>()) };
   // SAFETY: `ptr` is the trampoline stored for `F`.
   Some(unsafe { mem::transmute_copy(&ptr) })
}

unsafe extern "system" fn connect_detour(sock: usize, name: *const u8, len: i32) -> i32 {
   if bypassing() {
      return call_connect(sock, name, len);
   }
   match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      redirect(sock, name, len)
   })) {
      Ok(Some(true)) => 0,
      Ok(Some(false)) => fail_socket(),
      _ => call_connect(sock, name, len),
   }
}

unsafe extern "system" fn wsa_connect_detour(
   sock: usize,
   name: *const u8,
   len: i32,
   local_buf: *const u8,
   remote_buf: *mut u8,
   sqos: *const u8,
   gqos: *const u8,
) -> i32 {
   if bypassing() {
      return call_wsa_connect(sock, name, len, local_buf, remote_buf, sqos, gqos);
   }
   match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      redirect(sock, name, len)
   })) {
      Ok(Some(true)) => 0,
      Ok(Some(false)) => fail_socket(),
      _ => call_wsa_connect(sock, name, len, local_buf, remote_buf, sqos, gqos),
   }
}

unsafe extern "system" fn connect_ex_detour(
   sock: usize,
   name: *const u8,
   len: i32,
   send_buf: *const u8,
   send_len: u32,
   sent: *mut u32,
   overlapped: *mut c_void,
) -> i32 {
   if bypassing() {
      return call_connect_ex(sock, name, len, send_buf, send_len, sent, overlapped);
   }
   let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      redirect(sock, name, len)
   }));
   match handled {
      Ok(Some(true)) => {
         if !send_buf.is_null() && send_len > 0 {
            let bytes = unsafe { std::slice::from_raw_parts(send_buf, send_len as usize) };
            if !send_all(sock, bytes) {
               return fail_socket();
            }
            if !sent.is_null() {
               unsafe { sent.write(send_len) };
            }
         }
         // SAFETY: the socket is connected; this publishes that to Winsock
         // after a ConnectEx-shaped call completed inline.
         unsafe { setsockopt(sock, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, ptr::null(), 0) };
         1
      },
      Ok(Some(false)) => fail_socket(),
      _ => call_connect_ex(sock, name, len, send_buf, send_len, sent, overlapped),
   }
}

unsafe extern "system" fn ioctl_detour(
   sock: usize,
   code: u32,
   input: *const u8,
   input_len: u32,
   output: *mut u8,
   output_len: u32,
   returned: *mut u32,
   overlapped: *mut c_void,
   completion: *mut c_void,
) -> i32 {
   let rc = call_ioctl(
      sock,
      code,
      input,
      input_len,
      output,
      output_len,
      returned,
      overlapped,
      completion,
   );
   if rc != 0 || code != SIO_GET_EXTENSION || !connectex_guid(input, input_len) {
      return rc;
   }
   if output.is_null() || output_len < 8 {
      return rc;
   }
   let real = unsafe { ptr::read_unaligned(output.cast::<usize>()) };
   let detour = fn_addr(connect_ex_detour as *const ());
   if real != 0 && real != detour {
      let _ = CONNECT_EX.compare_exchange(0, real, Ordering::AcqRel, Ordering::Acquire);
      unsafe { ptr::write_unaligned(output.cast::<usize>(), detour) };
   }
   rc
}

unsafe extern "system" fn getaddrinfo_detour(
   node: *const u8,
   service: *const u8,
   hints: *const AddrInfo,
   out: *mut *mut AddrInfo,
) -> i32 {
   let rc = call_getaddrinfo(node, service, hints, out);
   if rc != 0 || bypassing() || node.is_null() || out.is_null() {
      return rc;
   }
   let host = unsafe { CStr::from_ptr(node.cast::<std::ffi::c_char>()) }.to_string_lossy();
   remember(&host, unsafe { *out });
   rc
}

unsafe extern "system" fn getaddrinfow_detour(
   node: *const u16,
   service: *const u16,
   hints: *const AddrInfo,
   out: *mut *mut AddrInfo,
) -> i32 {
   let rc = call_getaddrinfow(node, service, hints, out);
   if rc != 0 || bypassing() || node.is_null() || out.is_null() {
      return rc;
   }
   remember(&wide_cstr(node), unsafe { *out });
   rc
}

fn remember(host: &str, mut info: *mut AddrInfo) {
   let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut seen = 0;
      while !info.is_null() && seen < 32 {
         let current = unsafe { &*info };
         if let Some(ip) = ip_of(current.addr, current.addrlen) {
            super::note(host, ip);
         }
         info = current.next;
         seen += 1;
      }
   }));
}

fn redirect(sock: usize, name: *const u8, len: i32) -> Option<bool> {
   if !stream(sock) {
      return None;
   }
   let (ip, port) = endpoint(name, len)?;
   if port == 0 || local(ip) {
      return None;
   }
   let host = super::host_for(ip)?;
   let proxies = super::proxies(2);
   if proxies.is_empty() {
      return None;
   }
   for proxy in proxies {
      if dial(sock, &proxy, ip, port) {
         super::track(sock, &host);
         return Some(true);
      }
      super::fail(&proxy);
      if peered(sock) {
         return Some(false);
      }
   }
   None
}

fn dial(sock: usize, proxy: &str, ip: IpAddr, port: u16) -> bool {
   let Some(addr) = proxy_addr(proxy) else {
      return false;
   };
   let previous = (dword_opt(sock, SO_RCVTIMEO), dword_opt(sock, SO_SNDTIMEO));
   set_dword(sock, SO_RCVTIMEO, 5_000);
   set_dword(sock, SO_SNDTIMEO, 5_000);
   set_mode(sock, 0);
   let rc = call_connect(sock, addr.as_ptr(), i32::try_from(addr.len()).unwrap_or(16));
   let connected = if rc == 0 {
      true
   } else if last_error() == WSAEWOULDBLOCK {
      wait_connected(sock)
   } else {
      false
   };
   let ok = connected && socks::handshake(&mut Sock(sock), ip, port);
   set_mode(sock, 1);
   set_dword(sock, SO_RCVTIMEO, previous.0);
   set_dword(sock, SO_SNDTIMEO, previous.1);
   ok
}

fn wait_connected(sock: usize) -> bool {
   let mut poll = PollFd { sock, events: 0x10, revents: 0 };
   let ready = unsafe { WSAPoll(&raw mut poll, 1, 5_000) };
   ready > 0 && so_error(sock) == 0
}

fn endpoint(name: *const u8, len: i32) -> Option<(IpAddr, u16)> {
   if name.is_null() || len < 4 {
      return None;
   }
   let family = u16::from_le(unsafe { ptr::read_unaligned(name.cast::<u16>()) });
   if family == AF_INET && len >= 16 {
      let port = u16::from_be(unsafe { ptr::read_unaligned(name.add(2).cast::<u16>()) });
      let mut octets = [0u8; 4];
      unsafe { ptr::copy_nonoverlapping(name.add(4), octets.as_mut_ptr(), 4) };
      return Some((IpAddr::from(octets), port));
   }
   if family == AF_INET6 && len >= 28 {
      let port = u16::from_be(unsafe { ptr::read_unaligned(name.add(2).cast::<u16>()) });
      let mut octets = [0u8; 16];
      unsafe { ptr::copy_nonoverlapping(name.add(8), octets.as_mut_ptr(), 16) };
      return Some((IpAddr::from(octets), port));
   }
   None
}

fn proxy_addr(proxy: &str) -> Option<[u8; 16]> {
   let (ip, port) = proxy.rsplit_once(':')?;
   let ip: std::net::Ipv4Addr = ip.parse().ok()?;
   let port: u16 = port.parse().ok()?;
   let mut addr = [0u8; 16];
   addr[0] = 2;
   let port = port.to_be_bytes();
   addr[2] = port[0];
   addr[3] = port[1];
   addr[4..8].copy_from_slice(&ip.octets());
   Some(addr)
}

fn ip_of(addr: *const u8, len: usize) -> Option<IpAddr> {
   endpoint(addr, i32::try_from(len).unwrap_or(0)).map(|(ip, _)| ip)
}

const fn local(ip: IpAddr) -> bool {
   match ip {
      IpAddr::V4(addr) => addr.is_loopback() || addr.is_unspecified(),
      IpAddr::V6(addr) => addr.is_loopback() || addr.is_unspecified(),
   }
}

fn stream(sock: usize) -> bool {
   let mut kind = 0i32;
   let mut len = 4i32;
   let rc = unsafe { getsockopt(sock, SOL_SOCKET, SO_TYPE, (&raw mut kind).cast(), &raw mut len) };
   rc == 0 && kind == SOCK_STREAM
}

fn peered(sock: usize) -> bool {
   let mut addr = [0u8; 28];
   let mut len = 28i32;
   unsafe { getpeername(sock, addr.as_mut_ptr(), &raw mut len) == 0 }
}

fn so_error(sock: usize) -> i32 {
   let mut err = 0i32;
   let mut len = 4i32;
   let rc = unsafe { getsockopt(sock, SOL_SOCKET, SO_ERROR, (&raw mut err).cast(), &raw mut len) };
   if rc == 0 { err } else { -1 }
}

fn dword_opt(sock: usize, opt: i32) -> u32 {
   let mut value = 0u32;
   let mut len = 4i32;
   let rc = unsafe { getsockopt(sock, SOL_SOCKET, opt, (&raw mut value).cast(), &raw mut len) };
   if rc == 0 { value } else { 0 }
}

fn set_dword(sock: usize, opt: i32, value: u32) {
   unsafe { setsockopt(sock, SOL_SOCKET, opt, (&raw const value).cast(), 4) };
}

fn set_mode(sock: usize, nonblocking: u32) {
   let mut flag = nonblocking;
   unsafe { ioctlsocket(sock, FIONBIO, &raw mut flag) };
}

fn send_all(sock: usize, data: &[u8]) -> bool {
   let mut sent = 0;
   while sent < data.len() {
      let rest = i32::try_from(data.len() - sent).unwrap_or(i32::MAX);
      let n = unsafe { send(sock, data[sent..].as_ptr(), rest, 0) };
      if n <= 0 {
         return false;
      }
      sent += usize::try_from(n).unwrap_or(0);
   }
   true
}

fn recv_exact(sock: usize, data: &mut [u8]) -> bool {
   let mut got = 0;
   while got < data.len() {
      let rest = i32::try_from(data.len() - got).unwrap_or(i32::MAX);
      let n = unsafe { recv(sock, data[got..].as_mut_ptr(), rest, 0) };
      if n <= 0 {
         return false;
      }
      got += usize::try_from(n).unwrap_or(0);
   }
   true
}

fn fail_socket() -> i32 {
   unsafe { WSASetLastError(WSAECONNRESET) };
   -1
}

fn last_error() -> i32 {
   unsafe { WSAGetLastError() }
}

fn bypassing() -> bool {
   BYPASS.with(Cell::get)
}

fn call_connect(sock: usize, name: *const u8, len: i32) -> i32 {
   let Some(original) = load::<ConnectFn>(&CONNECT) else {
      return -1;
   };
   with_bypass(|| unsafe { original(sock, name, len) })
}

fn call_wsa_connect(
   sock: usize,
   name: *const u8,
   len: i32,
   local_buf: *const u8,
   remote_buf: *mut u8,
   sqos: *const u8,
   gqos: *const u8,
) -> i32 {
   let Some(original) = load::<WsaConnectFn>(&WSA_CONNECT) else {
      return -1;
   };
   with_bypass(|| unsafe { original(sock, name, len, local_buf, remote_buf, sqos, gqos) })
}

fn call_connect_ex(
   sock: usize,
   name: *const u8,
   len: i32,
   send_buf: *const u8,
   send_len: u32,
   sent: *mut u32,
   overlapped: *mut c_void,
) -> i32 {
   let Some(original) = load::<ConnectExFn>(&CONNECT_EX) else {
      unsafe { WSASetLastError(WSAEWOULDBLOCK) };
      return 0;
   };
   with_bypass(|| unsafe { original(sock, name, len, send_buf, send_len, sent, overlapped) })
}

#[expect(clippy::too_many_arguments, reason = "the argument list is WSAIoctl's")]
fn call_ioctl(
   sock: usize,
   code: u32,
   input: *const u8,
   input_len: u32,
   output: *mut u8,
   output_len: u32,
   returned: *mut u32,
   overlapped: *mut c_void,
   completion: *mut c_void,
) -> i32 {
   let Some(original) = load::<IoctlFn>(&IOCTL) else {
      return -1;
   };
   unsafe {
      original(
         sock,
         code,
         input,
         input_len,
         output,
         output_len,
         returned,
         overlapped,
         completion,
      )
   }
}

fn call_getaddrinfo(
   node: *const u8,
   service: *const u8,
   hints: *const AddrInfo,
   out: *mut *mut AddrInfo,
) -> i32 {
   let Some(original) = load::<GetAddrInfoFn>(&GETADDRINFO) else {
      return 8;
   };
   unsafe { original(node, service, hints, out) }
}

fn call_getaddrinfow(
   node: *const u16,
   service: *const u16,
   hints: *const AddrInfo,
   out: *mut *mut AddrInfo,
) -> i32 {
   let Some(original) = load::<GetAddrInfoWFn>(&GETADDRINFOW) else {
      return 8;
   };
   unsafe { original(node, service, hints, out) }
}

fn with_bypass<T>(body: impl FnOnce() -> T) -> T {
   BYPASS.with(|flag| {
      let previous = flag.replace(true);
      let value = body();
      flag.set(previous);
      value
   })
}

fn connectex_guid(input: *const u8, len: u32) -> bool {
   if input.is_null() || len < 16 {
      return false;
   }
   let mut guid = [0u8; 16];
   unsafe { ptr::copy_nonoverlapping(input, guid.as_mut_ptr(), 16) };
   guid == CONNECTEX_GUID
}

fn wide_cstr(text: *const u16) -> String {
   let mut len = 0usize;
   while len < 512 && unsafe { *text.add(len) } != 0 {
      len += 1;
   }
   String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) })
}
