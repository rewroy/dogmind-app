use socket2::{Domain, Socket, Type};
use std::net::{Ipv4Addr, SocketAddrV4};

/// Own a loopback port without accepting connections. Tests retain the socket while they expect refusals.
pub(crate) fn bound_loopback(port: u16) -> (Socket, u16) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    #[cfg(unix)]
    if port != 0 {
        // Rebind the server address even if a completed connection is in TIME_WAIT.
        socket.set_reuse_address(true).unwrap();
    }
    socket.bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into()).unwrap();
    #[cfg(unix)]
    if port != 0 {
        socket.set_reuse_address(false).unwrap();
    }
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    (socket, port)
}
