use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, UdpSocket},
    num::NonZeroUsize,
};

// We'll assume messages are in order and never lost...
// In this way we can send a series of messages, handle them in the other side,
// and expect an ordered return of results.

fn main() -> std::io::Result<()> {
    {
        let socket = UdpSocket::bind("127.0.0.1:34567")?;

        // Receives a single datagram message on the socket. If `buf` is too small to hold
        // the message, it will be cut off.
        let mut buf = [0; 100];
        let (amt, src) = socket.recv_from(&mut buf)?;

        let buf = &mut buf[..amt];
        let msg: ExampleMessage = serde_json::from_slice(buf).unwrap();
        println!("{}", msg.msg);
    } // the socket is closed here
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct MessageHeader {
    rpc: RpcName,
    body_size: usize,
    is_return: bool,
}
type MessageName = String;
type RpcName = String;

fn find_json_delimiter(buf: &[u8]) -> Option<NonZeroUsize> {
    let mut iter = buf.iter();
    if iter.next() == Some(&('{'.to_ascii_lowercase() as u8)) {
        let mut count = 1;
        let mut indent = 1;
        for b in iter {
            count += 1;
            // TODO/FIXME: Need to support detecting if { or } are inside a
            // string, or some number matches. Basically, need to parse json....
            if &('}'.to_ascii_lowercase() as u8) == b {
                indent -= 1;
                if indent == 0 {
                    return count.try_into().ok();
                }
            }
        }
    }
    None
}

trait MessageChannel {
    fn send(&mut self, buf: &[u8]) -> Result<usize, ()>;
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, ()>;
}

struct UDPChannel {
    socket: UdpSocket,
    dst: Option<SocketAddr>,
}
impl MessageChannel for UDPChannel {
    fn send(&mut self, buf: &[u8]) -> Result<usize, ()> {
        self.socket.send(buf).or(Err(()))
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, ()> {
        let (amt, dst) = self.socket.recv_from(buf).or(Err(()))?;
        if self.dst.is_none() {
            self.socket.connect(dst).unwrap();
        }
        Ok(amt)
    }
}

/// For example:
/// message ExampleMessage {
///     msg: string,
/// }
/// message ExampleReturn {
///     msg: string,
/// }
/// service Backend {
///     rpc ExampleMessage(ExampleMessage)
/// }

// Per Message:
#[derive(Serialize, Deserialize)]
pub struct ExampleMessage {
    pub msg: String,
}
#[derive(Serialize, Deserialize)]
pub struct ExampleReturn {
    pub msg: String,
}

// Per RPC:
pub enum BackendRpcArgVariant {
    ExampleRpc(ExampleMessage),
}
pub enum BackendRpcRetVariant {
    ExampleRpc(ExampleReturn),
}
pub const EXAMPLE_RPC_ID: &str = &"ExampleRpc";
/// User to implement handlers
trait BackendRpcHandler {
    fn handle_example_message(&mut self, msg: ExampleMessage) -> Result<ExampleReturn, ()>;
    fn handle_rpc_received(
        &mut self,
        arg: BackendRpcArgVariant,
    ) -> Result<BackendRpcRetVariant, ()> {
        match arg {
            BackendRpcArgVariant::ExampleRpc(m) => Ok(BackendRpcRetVariant::ExampleRpc(
                self.handle_example_message(m)?,
            )),
        }
    }
}

trait BackendServiceClient {
    fn call_example_message(&mut self, arg: ExampleMessage) -> Result<ExampleReturn, ()>;
    fn call(&mut self, arg: &BackendRpcArgVariant) -> Result<BackendRpcRetVariant, ()>;
}

trait BackendService {
    fn handler_loop<H: BackendRpcHandler>(&mut self, handler: H, addr: &str) -> Result<(), ()>;
}
impl<C: MessageChannel> BackendService for C {
    fn handler_loop<H: BackendRpcHandler>(&mut self, mut handler: H, addr: &str) -> Result<(), ()> {
        let mut buf = [0u8; 4096];
        loop {
            // TODO(error_handling) Listen for message
            let end = self.recv(&mut buf).unwrap();

            let mut start = 0;
            loop {
                if let Some((new_start, msg)) = BackendSerializer::parse_rpc_recv(&buf[start..end])
                {
                    start = new_start.get() + start;
                    // TODO(error_handling): Handle unexpected fail to parse more gracefully.
                    let res = handler
                        .handle_rpc_received(msg)
                        .expect("Unexpected failure to parse msg");

                    // TODO(error_handling): Eventually should queue up multiple returns rather than pushing individual messages.
                    let ret_buf = BackendSerializer::serialize_rpc_ret(res);
                    // TODO(error_handling): Gracefully handle failure to send response.
                    let sent = self.send(&ret_buf).unwrap();
                    assert_eq!(sent, ret_buf.len());
                } else {
                    break;
                }
            }
        }
    }
}

impl<C: MessageChannel> BackendServiceClient for C {
    fn call_example_message(&mut self, arg: ExampleMessage) -> Result<ExampleReturn, ()> {
        match self.call(&BackendRpcArgVariant::ExampleRpc(arg))? {
            BackendRpcRetVariant::ExampleRpc(res) => Ok(res),
            // TODO(error_handling): Unexpected result type received.
            _ => Err(()),
        }
    }

    // TODO(optimization) Add a queue option to the channel to support queueing
    // messages rather than sending one at a time. In this case, to handle the
    // return value, we would require them to provide a handler in the queue
    // submit() function. (Submit would then be responsible for parsing all
    // return values and calling the correct return handlers.)
    fn call(&mut self, arg: &BackendRpcArgVariant) -> Result<BackendRpcRetVariant, ()> {
        self.send(&BackendSerializer::serialize_rpc_arg(arg))
            .or(Err(()))?;

        // Now wait for response. (See optimize todo on function header, no need
        // to wait one.)
        let mut recv_buf = [0u8; 4096];
        let amt = self.recv(&mut recv_buf).or(Err(()))?;
        if let Some((_new_start, msg)) = BackendSerializer::parse_rpc_result(&recv_buf[..amt]) {
            Ok(msg)
        } else {
            Err(())
        }
    }
}

pub struct BackendSerializer;
impl BackendSerializer {
    /// Wire format:
    /// `[MessageHeader]` | `[ExampleMessage]`
    /// Header            | Body
    pub fn parse_rpc_recv(buf: &[u8]) -> Option<(NonZeroUsize, BackendRpcArgVariant)> {
        // Parse header
        let end = find_json_delimiter(buf)?.get();
        let header = serde_json::from_slice::<MessageHeader>(&buf[..end]).ok()?;
        assert!(!header.is_return);

        // Parse Body
        let start = end;
        let end = start + header.body_size;
        let msg = match header.rpc.as_str() {
            EXAMPLE_RPC_ID => BackendRpcArgVariant::ExampleRpc(
                serde_json::from_slice::<ExampleMessage>(&buf[start..end]).ok()?,
            ),
            // TODO(error_handling)
            _ => panic!("Unexpected rpc type"),
        };
        Some((end.try_into().ok()?, msg))
    }

    pub fn parse_rpc_result(buf: &[u8]) -> Option<(NonZeroUsize, BackendRpcRetVariant)> {
        // Parse header
        let end = find_json_delimiter(buf)?.get();
        let header = serde_json::from_slice::<MessageHeader>(&buf[..end]).ok()?;
        assert!(!header.is_return);

        // Parse Body
        let start = end;
        let end = start + header.body_size;
        let msg = match header.rpc.as_str() {
            EXAMPLE_RPC_ID => BackendRpcRetVariant::ExampleRpc(
                serde_json::from_slice::<ExampleReturn>(&buf[start..end]).ok()?,
            ),
            // TODO(error_handling)
            _ => panic!("Unexpected rpc type"),
        };
        Some((end.try_into().ok()?, msg))
    }

    pub fn serialize_rpc_arg(arg: &BackendRpcArgVariant) -> Vec<u8> {
        let mut body;
        let rpc_id;
        match arg {
            BackendRpcArgVariant::ExampleRpc(ref arg) => {
                body = serde_json::to_vec(arg).unwrap();
                rpc_id = EXAMPLE_RPC_ID;
            }
        }
        let header = MessageHeader {
            rpc: rpc_id.to_string(),
            body_size: body.len(),
            is_return: false,
        };
        let mut buf = serde_json::to_vec(&header).unwrap();
        buf.append(&mut body);
        buf
    }

    // TODO(optimization): Should support serializing into a buffer rather than
    // allocating (multiple) vecs.
    pub fn serialize_rpc_ret(ret: BackendRpcRetVariant) -> Vec<u8> {
        let mut body;
        let rpc_id;
        match ret {
            BackendRpcRetVariant::ExampleRpc(r) => {
                rpc_id = String::from(EXAMPLE_RPC_ID);
                body = serde_json::to_vec(&r).unwrap();
            }
        }

        let header = MessageHeader {
            rpc: rpc_id,
            body_size: body.len(),
            is_return: false,
        };
        let mut msg = serde_json::to_vec(&header).unwrap();
        msg.append(&mut body);
        return msg;
    }
}
