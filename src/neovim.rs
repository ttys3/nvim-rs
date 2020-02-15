//! An active neovim session.
use std::{
  future::Future,
  task::Poll,
  sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
  },
  pin::Pin,
};

use futures::{
  channel::{oneshot, mpsc::{unbounded, UnboundedReceiver, UnboundedSender}},
  io::{AsyncRead, AsyncWrite, BufWriter},
  lock::Mutex,
  poll,
  stream::StreamExt,
  sink::SinkExt,
};

use crate::{
  create::Spawner,
  error::{CallError, DecodeError, EncodeError, LoopError},
  rpc::{
    handler::Handler,
    model,
    model::{IntoVal, RpcMessage},
  },
  uioptions::UiAttachOptions,
};
use rmpv::Value;

/// Pack the given arguments into a `Vec<Value>`, suitable for using it for a
/// [`call`](crate::neovim::Neovim::call) to neovim.
#[macro_export]
macro_rules! call_args {
    () => (Vec::new());
    ($($e:expr), +,) => (call_args![$($e),*]);
    ($($e:expr), +) => {{
        let mut vec = Vec::new();
        $(
            vec.push($e.into_val());
        )*
        vec
    }};
}

type ResponseResult = Result<Result<Value, Value>, Arc<DecodeError>>;

type Queue = Arc<Mutex<Vec<(u64, oneshot::Sender<ResponseResult>)>>>;

/// An active Neovim session.
pub struct Neovim<W>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  pub(crate) writer: Arc<Mutex<BufWriter<W>>>,
  pub(crate) queue: Queue,
  pub(crate) msgid_counter: Arc<AtomicU64>,
}

impl<W> Clone for Neovim<W>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  fn clone(&self) -> Self {
    Neovim {
      writer: self.writer.clone(),
      queue: self.queue.clone(),
      msgid_counter: self.msgid_counter.clone(),
    }
  }
}

impl<W> Neovim<W>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  #[allow(clippy::new_ret_no_self)]
  pub fn new<H, R>(
    reader: R,
    writer: W,
    handler: H,
  ) -> (
    Neovim<<H as Handler>::Writer>,
    impl Future<Output = Result<(), Box<LoopError>>>,
    impl Future<Output = Result<(), Box<LoopError>>>,
  )
  where
    R: AsyncRead + Send + Unpin + 'static,
    H: Handler<Writer = W> + Spawner,
  {
    let req = Neovim {
      writer: Arc::new(Mutex::new(BufWriter::new(writer))),
      msgid_counter: Arc::new(AtomicU64::new(0)),
      queue: Arc::new(Mutex::new(Vec::new())),
    };

    let (sender, receiver) = unbounded();

    let req_1 = req.clone();
    let req_2 = req.clone();

    let fut1 = Self::io_loop(reader, sender, req_1);
    let fut2 = req_2.handle_rec(handler, receiver);

    (req, fut1, fut2)
  }

  async fn send_msg(
    &self,
    method: &str,
    args: Vec<Value>,
  ) -> Result<oneshot::Receiver<ResponseResult>, Box<EncodeError>> {
    let msgid = self.msgid_counter.fetch_add(1, Ordering::SeqCst);

    let req = RpcMessage::RpcRequest {
      msgid,
      method: method.to_owned(),
      params: args,
    };

    let (sender, receiver) = oneshot::channel();

    let mut q = self.queue.lock().await;
    eprintln!("[Id {}]: Queue before push: {:#?}", msgid, q);
    q.push((msgid, sender));
    eprintln!("[Id {}]: Queue after push: {:#?}", msgid, q);

    let writer = self.writer.clone();
    model::encode(writer, req).await?;

    eprintln!("[Id {}]: Receiver before return: {:#?}", msgid, receiver);
    Ok(receiver)
  }

  pub async fn call(
    &self,
    method: &str,
    args: Vec<Value>,
  ) -> Result<Result<Value, Value>, Box<CallError>> {
    let receiver = self
      .send_msg(method, args)
      .await
      .map_err(|e| CallError::SendError(*e, method.to_string()))?;

    eprintln!("Receiver before await: {:#?}", receiver);
    match receiver.await {
      // Result<Result<Result<Value, Value>, Arc<DecodeError>>, RecvError>
      Ok(Ok(r)) => Ok(r), // r is Result<Value, Value>, i.e. we got an answer
      Ok(Err(err)) => {
        // err is a Decode Error, i.e. the answer wasn't decodable
        Err(Box::new(CallError::DecodeError(err, method.to_string())))
      }
      Err(err) => {
        // err is RecvError
        Err(Box::new(CallError::InternalReceiveError(
          err,
          method.to_string(),
        )))
      }
    }
  }

  async fn send_error_to_callers(
    &self,
    queue: &Queue,
    err: DecodeError,
  ) -> Result<Arc<DecodeError>, Box<LoopError>> {
    let err = Arc::new(err);
    let mut v: Vec<u64> = vec![];

    let mut queue = queue.lock().await;
    queue.drain(0..).for_each(|sender| {
      let msgid = sender.0;
      sender
        .1
        .send(Err(err.clone()))
        .unwrap_or_else(|_| v.push(msgid));
    });

    if v.is_empty() {
      Ok(err)
    } else {
      Err((err, v).into())
    }
  }

  async fn handle_rec<H>(
    self,
    handler: H,
    mut receiver: UnboundedReceiver<RpcMessage>
  ) -> Result<(), Box<LoopError>>
    where
      H: Handler<Writer = W> + Spawner,
  {
    loop {
      let neovim = self.clone(); 
      neovim.handle_rec_next(&handler, &mut receiver).await?
    }
  }

  async fn handle_rec_next<H>(
    self,
    handler: &H,
    receiver: &mut UnboundedReceiver<RpcMessage>
  ) -> Result<(), Box<LoopError>>
    where
      H: Handler<Writer = W> + Spawner,
  {
    let msg = receiver.next().await.unwrap();
    self.handle_req_not(handler, msg, receiver).await?;
    Ok(())
  }

  fn handle_req_not<'a, H>(
    self,
    handler: &'a H,
    msg: RpcMessage,
    receiver: &'a mut UnboundedReceiver<RpcMessage>
  ) ->  Pin<Box<dyn Future<Output = Result<(), Box<LoopError>>> + Send + 'a>>
    where
      H: Handler<Writer = W> + Spawner,
  {
    Box::pin(async move {
      match msg {
        RpcMessage::RpcNotification { method, params } => {
          let neovim = self.clone();
          handler.handle_notify(method, params, neovim).await;
          Ok(())
        }
        RpcMessage::RpcRequest { msgid, method, params } => {
          loop {
            match poll!(handler.handle_request(method.clone(), params.clone(), self.clone())) {
              Poll::Ready(val) => {
                let response = match val {
                  Ok(result) => RpcMessage::RpcResponse {
                    msgid,
                    result,
                    error: Value::Nil,
                  },
                  Err(error) => RpcMessage::RpcResponse {
                    msgid,
                    result: Value::Nil,
                    error,
                  },
                };
                model::encode(self.writer.clone(), response)
                  .await.unwrap();
                  /*
                  .unwrap_or_else(|e| {
                    error!("Error sending response to request {}: '{}'", msgid, e);
                  });
                return Ok(());
                */
              }
              _ => {
                let neovim = self.clone();
                neovim.handle_rec_next(handler, receiver).await?
              }
            }
          }
        }
        _ => panic!(),
      }
    })
  }

  async fn io_loop<R>(
    mut reader: R,
    mut sender: UnboundedSender<RpcMessage>,
    neovim: Neovim<W>
  ) -> Result<(), Box<LoopError>>
  where
    R: AsyncRead + Send + Unpin + 'static,
  {
    let mut rest: Vec<u8> = vec![];

    loop {
      let msg = match model::decode(&mut reader, &mut rest).await {
        Ok(msg) => msg,
        Err(err) => {
          let e = neovim.send_error_to_callers(&neovim.queue, *err).await?;
          return Err(Box::new(LoopError::DecodeError(e, None)));
        }
      };

      debug!("Get message {:?}", msg);
      match msg {
        msg @ RpcMessage::RpcRequest {
          ..
        } => {
          sender.send(msg).await.unwrap();
        }
        RpcMessage::RpcResponse {
          msgid,
          result,
          error,
        } => {
          let sender = find_sender(&neovim.queue, msgid).await?;
          eprintln!("Sender: {:?}", sender);
          if error == Value::Nil {
            sender
              .send(Ok(Ok(result)))
              .map_err(|r| (msgid, r.expect("This was an OK(_)")))?;
          } else {
            sender
              .send(Ok(Err(error)))
              .map_err(|r| (msgid, r.expect("This was an OK(_)")))?;
          }
        }
        msg @ RpcMessage::RpcNotification { .. } => {
          sender.send(msg).await.unwrap();
        }
      }
    }
  }

  /// Register as a remote UI.
  ///
  /// After this method is called, the client will receive redraw notifications.
  pub async fn ui_attach(
    &mut self,
    width: i64,
    height: i64,
    opts: &UiAttachOptions,
  ) -> Result<(), Box<CallError>> {
    self
      .call(
        "nvim_ui_attach",
        call_args!(width, height, opts.to_value_map()),
      )
      .await?
      .map(|_| Ok(()))?
  }

  /// Send a quit command to Nvim.
  /// The quit command is 'qa!' which will make Nvim quit without
  /// saving anything.
  pub async fn quit_no_save(&mut self) -> Result<(), Box<CallError>> {
    self.command("qa!").await
  }
}

/* The idea to use Vec here instead of HashMap
 * is that Vec is faster on small queue sizes
 * in most cases Vec.len = 1 so we just take first item in iteration.
 */
async fn find_sender(
  queue: &Queue,
  msgid: u64,
) -> Result<oneshot::Sender<ResponseResult>, Box<LoopError>> {
  eprintln!("Looking for sender for id {}", msgid);
  let mut queue = queue.lock().await;

  let pos = match queue.iter().position(|req| req.0 == msgid) {
    Some(p) => p,
    None => return Err(msgid.into()),
  };
  eprintln!("Found sender at pos {}, queue length was {}", pos, queue.len());

  eprintln!("[Id {}]: Queue before sender removal: {:#?}", msgid, queue);
  let q = queue.remove(pos).1;
  eprintln!("[Id {}]: Queue after sender removal: {:#?}", msgid, queue);
  Ok(q)
}

#[cfg(all(test, feature = "use_tokio"))]
mod tests {
  use super::*;

  #[tokio::test]
  async fn test_find_sender() {
    let queue = Arc::new(Mutex::new(Vec::new()));

    {
      let (sender, _receiver) = oneshot::channel();
      queue.lock().await.push((1, sender));
    }
    {
      let (sender, _receiver) = oneshot::channel();
      queue.lock().await.push((2, sender));
    }
    {
      let (sender, _receiver) = oneshot::channel();
      queue.lock().await.push((3, sender));
    }

    find_sender(&queue, 1).await.unwrap();
    assert_eq!(2, queue.lock().await.len());
    find_sender(&queue, 2).await.unwrap();
    assert_eq!(1, queue.lock().await.len());
    find_sender(&queue, 3).await.unwrap();
    assert!(queue.lock().await.is_empty());

    if let LoopError::MsgidNotFound(17) =
      *find_sender(&queue, 17).await.unwrap_err()
    {
    } else {
      panic!()
    }
  }
}
