//! This is the actual Bot module. For ergonomic reasons there is a RcBot which composes the real bot
//! as an underlying field. You should always use RcBot. 

use objects;
use error::Error;

use std::str;
use std::io;
use std::time::Duration;
use std::collections::HashMap;
use std::rc::Rc;
use std::cell::{RefCell, Cell};

use tokio_core::reactor::{Handle, Core, Interval};
use serde_json;
use serde_json::value::Value;
use futures::{Future, IntoFuture, Stream, stream};
use futures::sync::mpsc;
use futures::sync::mpsc::UnboundedSender;
use hyper::client::Request;

use multipart::client::{Multipart};
use multipart::mock::{ClientRequest,HttpBuffer};
use mime::{Mime,TopLevel,SubLevel,Attr,Value as MimeValue};
use hyper::{self,Method,Url};
use hyper::header::{ContentType,ContentLength};
use hyper_tls;

/// A clonable, single threaded bot
///
/// The outer API gets implemented on RcBot
#[derive(Clone)]
pub struct RcBot {
    pub inner: Rc<Bot>
}

impl RcBot {
    pub fn new(handle: Handle, key: &str) -> RcBot {
        RcBot { inner: Rc::new(Bot::new(handle, key)) }
    }
}

/// The main bot structure
pub struct Bot {
    pub key: String,
    pub handle: Handle,
    pub last_id: Cell<u32>,
    pub update_interval: Cell<u64>,
    pub handlers: RefCell<HashMap<String, UnboundedSender<(RcBot, objects::Message)>>>,
    hyper_client: hyper::Client<hyper_tls::HttpsConnector>
}

impl Bot {
    pub fn new(handle: Handle, key: &str) -> Bot {
        Bot { 
            handle: handle.clone(), 
            key: key.into(), 
            last_id: Cell::new(0), 
            update_interval: Cell::new(1000), 
            handlers: RefCell::new(HashMap::new()), 
            hyper_client: hyper::Client::configure()
                .connector(hyper_tls::HttpsConnector::new(4, &handle))
                .build(&handle)
        }
    }

    /// Creates a new request and adds a JSON message to it. The returned Future contains a the
    /// reply as a string.  This method should be used if no file is added because a JSON msg is
    /// always compacter than a formdata one.
    pub fn fetch_json(&self, func: &str, msg: &str) -> impl Future<Item=String, Error=Error> {
        //debug!("Send JSON: {}", msg);
        
        let mut a = Request::new(Method::Post,Url::parse(&format!("https://api.telegram.org/bot{}/{}", self.key, func)).expect("Bad url"));
        a.headers_mut().set(ContentType(Mime(TopLevel::Application,SubLevel::Json,vec![])));
        a.set_body(msg.to_string());

        self._fetch(a)
    }

    fn build_multipart<T: io::Read>(msg: Value,mut file: T, kind: &str, file_name: &str) -> Result<HttpBuffer,::std::io::Error> {
          let mut mp = Multipart::from_request(ClientRequest::default())?;

          // add properties
          for (key, value) in msg.as_object().unwrap().iter() {
            mp.write_text(key,format!("{:?}",value))?;
          }
          // add the file
          mp.write_stream(kind,&mut file,Some(file_name),None)?;
          let buffer = mp.send()?;
          Ok(buffer)
    }

    /// Creates a new request with some byte content (e.g. a file). The method properties have to be 
    /// in the formdata setup and cannot be sent as JSON.
    pub fn fetch_formdata<T>(&self, func: &str, msg: Value, mut file: T, kind: &str, file_name: &str) -> impl Future<Item=String, Error=Error>  where T: io::Read {
        let mut content = Vec::new();

        let mut a = Request::new(Method::Post,Url::parse(&format!("https://api.telegram.org/bot{}/{}", self.key, func)).expect("Bad url"));
        
        // try to read the byte content to a vector
        let _ = file.read_to_end(&mut content).unwrap();

        let buffer = Self::build_multipart(msg,file,kind,file_name).expect("File IO failed?");

        a.headers_mut().set(ContentType(Mime(TopLevel::Multipart,SubLevel::FormData,vec![(Attr::Boundary,MimeValue::Ext(buffer.boundary))])));
        if let Some(len) = buffer.content_len{
          a.headers_mut().set(ContentLength(len));
        }

        // create a http post request
        a.set_body(buffer.buf);

        self._fetch(a)
    }

    /// calls hyper and parses the result for an error
    pub fn _fetch(&self, a: Request) -> impl Future<Item=String, Error=Error> {
        
        self.hyper_client.request(a)
        .and_then(|res|{
            res.body().fold(vec![],|mut buf,chunk|{
                buf.extend_from_slice(&*chunk);
                Result::Ok::<_,hyper::Error>(buf)
            })
        })
        .map_err(|_| Error::Hyper)
        .and_then(|body|{
            Ok(String::from_utf8_lossy(&*body).into_owned())
        })
        .and_then(|x| {
            // try to parse the result as a JSON and find the OK field.
            // If the ok field is true, then the string in "result" will be returned
            if let Ok(req) = serde_json::from_str::<Value>(&x) {
                if let (Some(ok), res) = (req.find("ok").and_then(Value::as_bool), req.find("result")) {
                    if ok {
                        if let Some(result) = res {
                            let answer = serde_json::to_string(result).unwrap();

                            return Ok(answer);
                        }
                    }
                    
                    match req.find("description").and_then(Value::as_str) {
                        Some(err) => Err(Error::Telegram(err.into())),
                        None => Err(Error::Telegram("Unknown".into()))
                    }
                } else {
                    return Err(Error::JSON);
                }
            } else {
                return Err(Error::JSON);
            }
        })
    }
}

impl RcBot {
    /// Sets the update interval to an integer in milliseconds
    pub fn update_interval(self, interval: u64) -> RcBot {
        self.inner.update_interval.set(interval);

        self
    }
   
    /// Creates a new command and returns a stream which will yield a message when the command is send
    pub fn new_cmd(&self, cmd: &str) -> impl Stream<Item=(RcBot,  objects::Message), Error=Error> {
        let (sender, receiver) = mpsc::unbounded();
    
        self.inner.handlers.borrow_mut().insert(cmd.into(), sender);

        receiver.map_err(|_| Error::Unknown)
    }

    /// Register a new commnd
    pub fn register<T>(&self, hnd: T) where T: Stream + 'static {
        self.inner.handle.spawn(hnd.for_each(|_| Ok(())).into_future().map(|_| ()).map_err(|_| ()));
    }
   
    /// The main update loop, the update function is called every update_interval milliseconds
    /// When an update is available the last_id will be updated and the message is filtered
    /// for commands
    /// The message is forwarded to the returned stream if no command was found
    pub fn get_stream<'a>(&'a self) -> impl Stream<Item=(RcBot, objects::Update), Error=Error> + 'a{
        use functions::*;

        Interval::new(Duration::from_millis(self.inner.update_interval.get()), &self.inner.handle).unwrap()
            .map_err(|_| Error::Unknown)
            .and_then(move |_| self.get_updates().offset(self.inner.last_id.get()).send())
            .map(|(_, x)| stream::iter(x.0.into_iter().map(|x| Ok(x)).collect::<Vec<Result<objects::Update, Error>>>()))
            .flatten()
            .and_then(move |x| {
                if self.inner.last_id.get() < x.update_id as u32 +1 {
                    self.inner.last_id.set(x.update_id as u32 +1);
                }
        
                Ok(x) 
            })
        .filter_map(move |mut val| {
            let mut forward: Option<String> = None;

            if let Some(ref mut message) = val.message {
                if let Some(text) = message.text.clone() {
                    let mut content = text.split_whitespace();
                    if let Some(cmd) = content.next() {
                        if self.inner.handlers.borrow_mut().contains_key(cmd) {
                            message.text = Some(content.collect::<Vec<&str>>().join(" "));

                            forward = Some(cmd.into());
                        }
                    }
                }
            }
           
            if let Some(cmd) = forward {
                if let Some(sender) = self.inner.handlers.borrow_mut().get_mut(&cmd) {
                    sender.send((self.clone(), val.message.unwrap())).unwrap();
                }
                return None;
            } else {
                return Some((self.clone(), val));
            }
        })
    }
   
    /// helper function to start the event loop
    pub fn run<'a>(&'a self, core: &mut Core) -> Result<(), Error> {
        core.run(self.get_stream().for_each(|_| Ok(())).into_future())      
    }
}
