use std::{
   io::{
      Read,
      Write,
   },
   net::{
      IpAddr,
      TcpStream,
   },
};

pub trait Bytes {
   fn push(&mut self, data: &[u8]) -> bool;
   fn pull(&mut self, data: &mut [u8]) -> bool;
}

impl Bytes for TcpStream {
   fn push(&mut self, data: &[u8]) -> bool {
      self.write_all(data).is_ok()
   }

   fn pull(&mut self, data: &mut [u8]) -> bool {
      self.read_exact(data).is_ok()
   }
}

pub fn handshake(io: &mut impl Bytes, ip: IpAddr, port: u16) -> bool {
   greet_and_connect(io, &connect_packet(ip, port))
}

pub fn handshake_host(io: &mut impl Bytes, host: &str, port: u16) -> bool {
   if host.len() > 255 || host.is_empty() || !host.is_ascii() {
      return false;
   }
   let mut request = vec![5, 1, 0, 3, u8::try_from(host.len()).unwrap_or(0)];
   request.extend(host.as_bytes());
   request.extend(port.to_be_bytes());
   greet_and_connect(io, &request)
}

fn greet_and_connect(io: &mut impl Bytes, request: &[u8]) -> bool {
   if !io.push(&[5, 1, 0]) {
      return false;
   }
   let mut chosen = [0u8; 2];
   if !io.pull(&mut chosen) || chosen != [5, 0] {
      return false;
   }
   if !io.push(request) {
      return false;
   }
   accept_reply(io)
}

fn accept_reply(io: &mut impl Bytes) -> bool {
   let mut head = [0u8; 4];
   if !io.pull(&mut head) || head[0] != 5 || head[1] != 0 {
      return false;
   }
   let rest = match head[3] {
      1 => 6,
      4 => 18,
      3 => {
         let mut len = [0u8; 1];
         if !io.pull(&mut len) {
            return false;
         }
         usize::from(len[0]) + 2
      },
      _ => return false,
   };
   let mut tail = vec![0u8; rest];
   io.pull(&mut tail)
}

pub fn connect_packet(ip: IpAddr, port: u16) -> Vec<u8> {
   let port = port.to_be_bytes();
   let mut packet = vec![5, 1, 0];
   match ip {
      IpAddr::V4(addr) => {
         packet.push(1);
         packet.extend(addr.octets());
      },
      IpAddr::V6(addr) => {
         packet.push(4);
         packet.extend(addr.octets());
      },
   }
   packet.extend(port);
   packet
}

#[cfg(test)]
mod tests {
   use std::net::Ipv4Addr;

   use super::connect_packet;

   #[test]
   fn v4_connect_packet() {
      let packet = connect_packet(Ipv4Addr::new(1, 2, 3, 4).into(), 443);
      assert_eq!(packet, [5, 1, 0, 1, 1, 2, 3, 4, 0x01, 0xBB]);
   }
}
