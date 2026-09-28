//! The UDP doors' buffer sizes reach the socket (BACKLOG B4): read back
//! with `getsockopt`, against the kernel's own rule (Linux doubles a
//! request up to `net.core.{r,w}mem_max`).

use super::*;

fn any_port() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// The kernel's cap for one direction (`rmem_max` / `wmem_max`).
#[cfg(target_os = "linux")]
fn sysctl(name: &str) -> usize {
    std::fs::read_to_string(format!("/proc/sys/net/core/{name}"))
        .expect("readable sysctl")
        .trim()
        .parse()
        .expect("a number")
}

/// Two sizes per direction, both under the kernel's cap: one below the
/// system default (a knob that only ever raised the buffer would pass a
/// larger-only check) and one at the cap. Linux reads each back doubled,
/// exactly.
#[cfg(target_os = "linux")]
#[test]
fn a_requested_size_reaches_the_socket_doubled() {
    let (rmax, wmax) = (sysctl("rmem_max") as u32, sysctl("wmem_max") as u32);
    for (recv, send) in [(16_384, 20_480), (rmax, wmax)] {
        let asked = UdpBuffers {
            recv: Some(recv),
            send: Some(send),
        };
        let sock = bind_udp(any_port(), asked).expect("bind");
        let (r, s) = buffer_sizes(&sock).expect("getsockopt");
        assert_eq!(r, 2 * recv as usize, "receive buffer for {recv}");
        assert_eq!(s, 2 * send as usize, "send buffer for {send}");
        assert!(!capped(recv, r) && !capped(send, s), "granted in full");
    }
}

/// Past the cap the request is not refused: the kernel grants the cap
/// (doubled), and the door's warning rule sees the shortfall.
#[cfg(target_os = "linux")]
#[test]
fn a_request_past_the_cap_is_capped_and_detected() {
    let rmax = sysctl("rmem_max");
    let asked = (rmax * 2).min(MAX_SOCKET_BUFFER as usize) as u32;
    let sock = bind_udp(
        any_port(),
        UdpBuffers {
            recv: Some(asked),
            send: None,
        },
    )
    .expect("a capped request still binds");
    let (r, _) = buffer_sizes(&sock).expect("getsockopt");
    assert_eq!(r, 2 * rmax, "capped at rmem_max, then doubled");
    assert!(capped(asked, r), "the shortfall is detected");
}

/// Unset means untouched: the sizes a plain `std` bind gets.
#[test]
fn unset_sizes_keep_the_system_default() {
    let ours = bind_udp(any_port(), UdpBuffers::default()).expect("bind");
    let plain = std::net::UdpSocket::bind(any_port()).expect("bind");
    assert_eq!(
        buffer_sizes(&ours).expect("getsockopt"),
        buffer_sizes(&plain).expect("getsockopt")
    );
}

/// Below a page and past a C `int` never reach a socket, in either
/// direction; the bounds themselves are accepted.
#[test]
fn out_of_range_sizes_are_refused() {
    for bad in [0, 1, MIN_SOCKET_BUFFER - 1, MAX_SOCKET_BUFFER + 1, u32::MAX] {
        assert!(socket_buffer_problem(bad).is_some(), "{bad}");
        for asked in [
            UdpBuffers {
                recv: Some(bad),
                send: None,
            },
            UdpBuffers {
                recv: None,
                send: Some(bad),
            },
        ] {
            let err = bind_udp(any_port(), asked).expect_err("refused");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad}: {err}");
        }
    }
    for good in [MIN_SOCKET_BUFFER, 1 << 20, MAX_SOCKET_BUFFER] {
        assert_eq!(socket_buffer_problem(good), None, "{good}");
    }
}

/// The socket is non-blocking (tokio's `from_std` and quinn's runtime
/// both require it) and bound where asked.
#[tokio::test]
async fn the_socket_is_ready_for_tokio() {
    let std_sock = bind_udp(any_port(), UdpBuffers::default()).expect("bind");
    let sock = tokio::net::UdpSocket::from_std(std_sock).expect("registers");
    let mut buf = [0u8; 1];
    let err = sock.try_recv_from(&mut buf).expect_err("nothing queued");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    assert!(sock.local_addr().expect("bound").port() != 0);
}
