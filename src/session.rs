
use time::PreciseTime;
use channel;
use connection::Connection;
use protocol;
use amq_proto::{self, Table, Method, Frame, MethodFrame, FrameType, FramePayload};
use amq_proto::TableEntry::{FieldTable, Bool, LongString};

use amqp_error::{AMQPResult, AMQPError};
use super::VERSION;

use std::io::ErrorKind;
use std::sync::{Arc, Mutex};
use std::default::Default;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use std::time::Duration;
use std::thread;
use std::cmp;

use url::{Url, percent_encoding};

pub const AMQPS_PORT: u16 = 5671;
pub const AMQP_PORT: u16 = 5672;

const CHANNEL_BUFFER_SIZE: usize = 100;

#[derive(Debug)]
pub enum AMQPScheme {
    AMQP,
    #[cfg(feature = "tls")]
    AMQPS,
}

pub enum EventFrame {
    Frame(Frame),
    FrameMaxLimit(u32),
}

#[derive(Debug)]
pub struct Options {
    pub host: String,
    pub port: u16,
    pub login: String,
    pub password: String,
    pub vhost: String,
    pub frame_max_limit: u32,
    pub channel_max_limit: u16,
    pub heartbeat: u16,
    pub locale: String,
    pub scheme: AMQPScheme,
    pub properties: Table,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            host: "127.0.0.1".to_string(),
            port: AMQP_PORT,
            vhost: "".to_string(),
            login: "guest".to_string(),
            password: "guest".to_string(),
            frame_max_limit: 131072,
            channel_max_limit: 65535,
            heartbeat: 15,
            locale: "en_US".to_string(),
            scheme: AMQPScheme::AMQP,
            properties: Table::new(),
        }
    }
}

pub struct Session {
    channels: Arc<Mutex<HashMap<u16, SyncSender<AMQPResult<Frame>>>>>,
    send_sender: SyncSender<EventFrame>,
    channel_max_limit: u16,
    heartbeat: u16,
    channel_zero: channel::Channel,
}

impl Session {
    /// Use `open_url` to create new amqp session from a "amqp url"
    ///
    /// # Arguments
    /// * `url_string`: The format is: `amqp://username:password@host:port/virtual_host`
    ///
    /// Most of the params have their default, so you can just pass this:
    /// `"amqp://localhost//"` and it will connect to rabbitmq server,
    /// running on `localhost` on port `5672`,
    /// with login `"guest"`, password: `"guest"` to vhost `"/"`
    pub fn open_url(url_string: &str) -> AMQPResult<Session> {
        let options = try!(parse_url(url_string));
        Session::new(options)
    }

    /// Initialize new rabbitmq session.
    /// You can use default options:
    /// # Example
    /// ```no_run
    /// use std::default::Default;
    /// use amqp::{Options, Session};
    /// let session = match Session::new(Options { .. Default::default() }){
    ///     Ok(session) => session,
    ///     Err(error) => panic!("Failed openning an amqp session: {:?}", error)
    /// };
    /// ```
    pub fn new(options: Options) -> AMQPResult<Session> {
        let connection = try!(get_connection(&options));

        let (send_sender, send_receiver) = sync_channel(CHANNEL_BUFFER_SIZE); //writes

        let channels = Arc::new(Mutex::new(HashMap::new()));
        let (channel_zero_sender, channel_receiver) = sync_channel(CHANNEL_BUFFER_SIZE); //channel0
        let channel_zero = channel::Channel::new(0, channel_receiver, send_sender.clone());
        try!(channels.lock().map_err(|_| AMQPError::SyncError)).insert(0, channel_zero_sender);

        let (heartbeat_dead_sender, heartbeat_dead_receiver) = sync_channel(CHANNEL_BUFFER_SIZE);
        {
            let sender = send_sender.clone();
            let heartbeat = options.heartbeat;
            Some(thread::spawn(move || {
                Session::heartbeat_loop(
                    sender,
                    Duration::from_secs(heartbeat as u64)
                );
                heartbeat_dead_sender.send(());
            }));
        }

        {
            let channels_clone = channels.clone();
            thread::spawn(|| Session::reading_loop(connection, channels_clone, send_receiver, heartbeat_dead_receiver));
        }

        let mut session = Session {
            channels: channels,
            send_sender: send_sender,
            channel_max_limit: 65535,
            heartbeat: options.heartbeat,
            channel_zero: channel_zero,
        };

        try!(session.init(options));



        Ok(session)
    }

    fn init(&mut self, options: Options) -> AMQPResult<()> {
        debug!("Starting init session");
        let frame = try!(self.channel_zero.read()); //Start
        let method_frame = try!(MethodFrame::decode(&frame));
        let start: protocol::connection::Start = match method_frame.method_name() {
            "connection.start" => try!(Method::decode(method_frame)),
            meth => return Err(AMQPError::Protocol(format!("Unexpected method frame: {:?}", meth))),
        };
        debug!("Received connection.start: {:?}", start);
        // * The client selects a security mechanism (Start-Ok).
        // * The server starts the authentication process, which uses the SASL
        // challenge-response model. It sends
        // the client a challenge (Secure).
        // * The client sends an authentication response (Secure-Ok). For example using
        // the "plain" mechanism,
        // the response consist of a login name and password.
        // * The server repeats the challenge (Secure) or moves to negotiation, sending
        // a set of parameters such as

        let mut client_properties = Table::new();
        let mut capabilities = Table::new();
        capabilities.insert("publisher_confirms".to_owned(), Bool(true));
        capabilities.insert("consumer_cancel_notify".to_owned(), Bool(true));
        capabilities.insert("exchange_exchange_bindings".to_owned(), Bool(true));
        capabilities.insert("basic.nack".to_owned(), Bool(true));
        capabilities.insert("connection.blocked".to_owned(), Bool(true));
        capabilities.insert("authentication_failure_close".to_owned(), Bool(true));
        client_properties.insert("capabilities".to_owned(), FieldTable(capabilities));
        client_properties.insert("product".to_owned(), LongString("rust-amqp".to_owned()));
        client_properties.insert("platform".to_owned(), LongString("rust".to_owned()));
        client_properties.insert("version".to_owned(), LongString(VERSION.to_owned()));
        client_properties.insert("information".to_owned(),
                                 LongString("https://github.com/Antti/rust-amqp".to_owned()));
        client_properties.extend(options.properties);

        debug!("Sending connection.start-ok");
        let start_ok = protocol::connection::StartOk {
            client_properties: client_properties,
            mechanism: "PLAIN".to_owned(),
            response: format!("\0{}\0{}", options.login, options.password),
            locale: options.locale.to_owned(),
        };
        let response = try!(self.channel_zero.raw_rpc(&start_ok));
        let tune: protocol::connection::Tune = match response.method_name() {
            "connection.tune" => try!(amq_proto::Method::decode(response)),
            "connection.close" => {
                let close_frame: protocol::connection::Close =
                    try!(amq_proto::Method::decode(response));
                return Err(AMQPError::Protocol(format!("Connection was closed: {:?}",
                                                       close_frame)));
            }
            response_method => {
                return Err(AMQPError::Protocol(format!("Unexpected response: {}", response_method)))
            }
        };
        debug!("Received tune request: {:?}", tune);

        self.channel_max_limit = negotiate(tune.channel_max, self.channel_max_limit);
        self.heartbeat = negotiate(tune.heartbeat, self.heartbeat);
        let frame_max_limit = negotiate(tune.frame_max, options.frame_max_limit);
        self.channel_zero.set_frame_max_limit(frame_max_limit);

        let tune_ok = protocol::connection::TuneOk {
            channel_max: self.channel_max_limit,
            frame_max: frame_max_limit,
            heartbeat: self.heartbeat,
        };
        debug!("Sending connection.tune-ok: {:?}", tune_ok);
        try!(self.channel_zero.send_method_frame(&tune_ok));

        let open = protocol::connection::Open {
            virtual_host: percent_decode(&options.vhost),
            capabilities: "".to_owned(),
            insist: false,
        };
        debug!("Sending connection.open: {:?}", open);
        let open_ok = self.channel_zero
            .rpc::<_, protocol::connection::OpenOk>(&open, "connection.open-ok");
        match open_ok {
            Ok(_) => {
                debug!("Connection initialized. connection.open-ok received");
                info!("Session initialized");
                Ok(())
            }
            Err(AMQPError::FramingError(_)) => Err(AMQPError::VHostError),
            Err(other_error) => Err(other_error),
        }
    }

    /// `open_channel` will open a new amqp channel:
    /// # Arguments
    ///
    /// * `channel_id` - channel number
    ///
    /// # Exmaple
    /// ```no_run
    /// use std::default::Default;
    /// use amqp::{Options, Session};
    /// let mut session = Session::new(Options { .. Default::default() }).ok().unwrap();
    /// let channel = match session.open_channel(1){
    ///     Ok(channel) => channel,
    ///     Err(error) => panic!("Failed openning channel: {:?}", error)
    /// };
    /// ```
    pub fn open_channel(&mut self, channel_id: u16) -> AMQPResult<channel::Channel> {
        debug!("Openning channel: {}", channel_id);
        let (sender, receiver) = sync_channel(CHANNEL_BUFFER_SIZE);
        let mut channel = channel::Channel::new(channel_id, receiver, self.send_sender.clone());
        try!(self.channels.lock().map_err(|_| AMQPError::SyncError)).insert(channel_id, sender);
        try!(channel.open());
        Ok(channel)
    }

    pub fn close<T>(&mut self, reply_code: u16, reply_text: T)
        where T: Into<String>
    {
        let reply_text = reply_text.into();
        debug!("Closing session: reply_code: {}, reply_text: {}",
               reply_code,
               reply_text);
        let close = protocol::connection::Close {
            reply_code: reply_code,
            reply_text: reply_text,
            class_id: 0,
            method_id: 0,
        };
        let _: protocol::connection::CloseOk = self.channel_zero
            .rpc(&close, "connection.close-ok")
            .ok()
            .unwrap();
    }

    /// Send heartbeats every few seconds to make sure we're up.
    /// This function will never return Ok(()), but Result is used
    /// to have the compiler warn about not using it.
    fn heartbeat_loop(
        chan: SyncSender<EventFrame>,
        delay: Duration
    ) -> Result<(), ()> {
        loop {
            thread::sleep(delay);
            trace!("Sending heartbeat");
            if chan.send(EventFrame::Frame(Frame {
                frame_type: FrameType::HEARTBEAT,
                channel: 0,
                payload: FramePayload::new(vec![]),
            })).is_err() {
                return Err(());
            }
        }
    }

    // Receives and dispatches frames from the connection to the corresponding
    // channels.
    fn reading_loop(mut connection: Connection,
                    channels: Arc<Mutex<HashMap<u16, SyncSender<AMQPResult<Frame>>>>>,
                    send_receiver: Receiver<EventFrame>,
                    heartbeat_died_receiver: Receiver<()>
    )
                    -> () {
        debug!("Starting reading loop");
        loop {
            trace!("Start of the read loop");

            if heartbeat_died_receiver.try_recv().is_ok() {
                warn!("Heartbeat failed");
                let chans = channels.lock().unwrap();
                for chan in chans.values() {
                    // Propagate error to every channel, so they can close
                    if chan.send(Err(AMQPError::HeartbeatDead)).is_err() {
                        error!("Error dispatching closing packet to a channel");
                    }
                }
                break;
            }

            let mut sent = 0;
            while sent < 100 {
                let recvd = send_receiver.try_recv();
                if let Ok(frame) = recvd {
                    sent += 1;
                    match frame {
                        EventFrame::Frame(frame) => {
                            trace!("Writing frame: {:?}", frame);
                            if let Err(e) = connection.write(frame) {
                                let chans = channels.lock().unwrap();
                                for chan in chans.values() {
                                    // Propagate error to every channel, so they can close
                                    if chan.send(Err(e.clone())).is_err() {
                                        error!("Error dispatching closing packet to a channel");
                                    }
                                }
                            }
                        }
                        EventFrame::FrameMaxLimit(limit) => {
                            trace!("Max frame limit: {:?}", limit);
                            connection.frame_max_limit = limit;
                        }
                    }
                } else {
                    break;
                }
            }

            debug!("Sent {:?} frames", sent);

            let start = PreciseTime::now();

            match connection.read() {
                Ok(frame) => {
                    trace!("got frame: {:?}", frame);
                    // TODO: If channel 0 -> send to channel_zero_handler
                    // If channel != 0 and FrameType == METHOD and method class =='connection',
                    // then reply code 503 (command invalid).
                    let chans = channels.lock().unwrap();
                    let chan_id = frame.channel;
                    let target = chans.get(&chan_id);

                    let mut forward = true;

                    if chan_id == 0 {
                        match frame.frame_type {
                            FrameType::HEARTBEAT => {
                                debug!("Received heartbeat");
                                forward = false;
                            },
                            _ => {
                                info!(
                                    "Frame to channel 0: {:?}",
                                    String::from_utf8_lossy(frame.payload.inner())
                                );
                            }
                        }
                    }


                    match target {
                        Some(target_channel) => {
                            if forward {
                                match target_channel.try_send(Ok(frame)) {
                                    Ok(()) => {},
                                    Err(TrySendError::Disconnected(frame)) => {
                                        warn!(
                                            "Error dispatching packet to channel {}: Receiver is gone.",
                                            &chan_id
                                        );
                                    },
                                    Err(TrySendError::Full(frame)) => {
                                        warn!(
                                            "Error dispatching packet to channel {}: Full! Blocking until there is space.",
                                            &chan_id
                                        );

                                        if let Err(err) = target_channel.send(frame) {
                                            warn!(
                                                "Error dispatching packet to channel {}, Even after waiting for a blocked write: {:?}",
                                                &chan_id,
                                                err
                                            );
                                        }
                                    }
                                }
                            }
                        },
                        None => error!("Received frame for an unknown channel: {}", chan_id),
                    }
                }
                Err(read_err) => {
                    trace!("Error in reading loop: {:?}", read_err);

                    let mut send_err = false;

                    match &read_err {
                        &AMQPError::IoError(ErrorKind::WouldBlock) => {},
                        &AMQPError::IoError(kind) => {
                            error!("Connection IOError ({:?}): {:?}", kind, read_err);
                            send_err = true;
                        },
                        ref e => {
                            error!("Mystery error! {:?}", e);
                            send_err = true;
                        }
                    }

                    if send_err {
                        let chans = channels.lock().unwrap();
                        for chan in chans.values() {
                            // Propagate error to every channel, so they can close
                            if chan.send(Err(read_err.clone())).is_err() {
                                error!("Error dispatching closing packet to a channel");
                            }
                        }
                        break;
                    }
                }
            }

            debug!("In the read portion of the loop for: {:?}", start.to(PreciseTime::now()));

        }
        debug!("Exiting reading loop");
    }
}

fn get_connection(options: &Options) -> AMQPResult<Connection> {
    match options.scheme {
        #[cfg(feature = "tls")]
        AMQPScheme::AMQPS => Connection::open_tls(&options.host, options.port).map_err(From::from),
        AMQPScheme::AMQP => Connection::open(&options.host, options.port).map_err(From::from),
    }
}
fn negotiate<T: cmp::Ord>(their_value: T, our_value: T) -> T {
    cmp::min(their_value, our_value)
}

fn percent_decode(string: &str) -> String {
    percent_encoding::percent_decode(string.as_bytes()).decode_utf8_lossy().to_string()
}

fn parse_url(url_string: &str) -> AMQPResult<Options> {
    fn clean_vhost(vhost: &str) -> &str {
        match vhost.chars().next() {
            Some('/') => &vhost[1..],
            _ => vhost
        }
    }

    let default: Options = Default::default();

    let url = try!(Url::parse(url_string));
    if url.cannot_be_a_base() {
        return Err(AMQPError::SchemeError("Must have relative scheme".to_string()));
    }

    let vhost = clean_vhost(url.path());
    let host = url.host().map(|s| s.to_string()).unwrap_or(default.host);
    let login = match url.username() {
        "" => String::from(default.login),
        username => username.to_string()
    };
    let password = url.password().map_or(String::from(default.password), ToString::to_string);
    let (scheme, default_port) = match url.scheme() {
        "amqp" => (AMQPScheme::AMQP, AMQP_PORT),
        #[cfg(feature = "tls")]
        "amqps" => (AMQPScheme::AMQPS, AMQPS_PORT),
        unknown_scheme => {
            return Err(AMQPError::SchemeError(format!("Unknown scheme: {:?}", unknown_scheme)))
        }
    };
    let port = url.port().unwrap_or(default_port);

    Ok(Options {
        host: host.to_string(),
        port: port,
        scheme: scheme,
        login: login,
        password: password,
        vhost: vhost.to_string(),
        ..Default::default()
    })
}

#[cfg(test)]
mod test {
    use super::{parse_url, AMQPScheme};

    #[test]
    fn test_full_parse_url() {
        let options = parse_url("amqp://username:password@hostname:12345/vhost").expect("Failed parsing url");
        assert_eq!(options.host, "hostname");
        assert_eq!(options.login, "username");
        assert_eq!(options.password, "password");
        assert_eq!(options.port, 12345);
        assert_eq!(options.vhost, "vhost");
        assert!(match options.scheme { AMQPScheme::AMQP => true, _ => false });
    }

    #[test]
    fn test_full_parse_url_with_ip() {
        let options = parse_url("amqp://username:password@123.123.123.123:12345/vhost").expect("Failed parsing url");
        assert_eq!(options.host, "123.123.123.123");
        assert_eq!(options.login, "username");
        assert_eq!(options.password, "password");
        assert_eq!(options.port, 12345);
        assert_eq!(options.vhost, "vhost");
        assert!(match options.scheme { AMQPScheme::AMQP => true, _ => false });
    }

    #[test]
    fn test_full_parse_url_without_vhost() {
        let options = parse_url("amqp://host").expect("Failed parsing url");
        assert_eq!(options.host, "host");
        assert_eq!(options.vhost, "");
    }

    #[test]
    fn test_full_parse_url_with_empty_vhost() {
        let options = parse_url("amqp://host/").expect("Failed parsing url");
        assert_eq!(options.host, "host");
        assert_eq!(options.vhost, "");
    }

    #[test]
    fn test_full_parse_url_with_slash_vhost() {
        let options = parse_url("amqp://host//").expect("Failed parsing url");
        assert_eq!(options.host, "host");
        assert_eq!(options.vhost, "/");
    }

    #[test]
    fn test_parse_url_defaults() {
        let options = parse_url("amqp://").expect("Failed parsing url");
        assert_eq!(options.host, "");
        assert_eq!(options.vhost, "");
        assert_eq!(options.login, "guest");
        assert_eq!(options.password, "guest");
        assert_eq!(options.port, 5672);
        assert_eq!(options.heartbeat, 15);
    }
}
