use std::io::Write;

use peace_performance::{Beatmap, BeatmapExt};
use reqwest;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

#[derive(Eq, Hash, PartialEq, Debug)]
pub enum EventType {
    AuthSuccess,
    AuthFailed,
    MotdStart,
    Motd,
    MotdEnd,
    Quit,
    PrivMsg,
    Ping,
    Error,
}

#[derive(Debug)]
pub struct BanchoEvent {
    pub event_type: EventType,
    pub sender: String,
    pub receiver: String,
    pub message: String,
}

pub struct Connection {
    event_handlers: Vec<Box<dyn Fn(&BanchoEvent) + Send + Sync + 'static>>,
    reader: BufReader<TcpStream>,
    username: String,
}

impl Connection {
    pub async fn new(username: String, irc_token: String) -> Self {
        // create the stream
        let stream = init_stream().await;
        // username needs formatting for the irc auth message
        let username_auth_format = username.replace(" ", "_");
        // auth message
        let login = format!("PASS {}\r\nNICK {}\r\n", irc_token, username_auth_format);
        // create the connection
        let mut connection = Self {
            username,
            event_handlers: Vec::new(),
            reader: BufReader::new(stream),
        };
        // send auth message
        connection.send_bancho(login).await;
        // return the connection
        connection
    }

    pub async fn listen(&mut self) {
        let mut line = String::new();
        loop {
            // prepare buffer for reading
            line.clear();
            // read lines from the server
            self.reader.read_line(&mut line).await.unwrap();
            // skip empty lines
            if line.is_empty() {
                continue;
            }
            // parse the line
            let event = self.parse_line(line.clone());
            // emit the event
            self.emit_event(&event.await).await;
        }
    }

    pub fn register_event<F>(&mut self, event_type: EventType, handler: F)
    where
        F: Fn(&BanchoEvent) + Send + Sync + 'static,
    {
        self.event_handlers.push(Box::new(move |event| {
            if event.event_type == event_type {
                handler(event);
            }
        }));
    }

    pub async fn emit_event(&mut self, event: &BanchoEvent) {
        match event.event_type {
            EventType::Error => println!("EventType::Error thrown with buffer: {}", event.message),
            EventType::Ping => {
                // reply PONG to PING to maintain the connection
                let pong_message = event.message.replace("PING", "PONG");
                self.send_bancho(pong_message).await;
            }
            _ => (),
        }
        for handler in &self.event_handlers {
            handler(event);
        }
    }

    async fn parse_line(&self, line: String) -> BanchoEvent {
        if line.starts_with("PING") {
            return BanchoEvent {
                event_type: EventType::Ping,
                sender: String::new(),
                receiver: String::new(),
                message: line,
            };
        }

        // most bancho communications are in this format
        // :Tillerino!cho@ppy.sh PRIVMSG Auracle :You really look terrible today you should try sunscream...\r\n
        // the first part is the sender, the second part is the command and the rest depends on the command
        // except for PING messages that look like this
        // PING cho.ppy.sh\r\n
        let split_line = line.clone();
        let mut split_line = split_line.split(' ');
        let receiver = self.username.clone();
        // get the first art example :Tillerino!cho@ppy.sh
        let mut sender = split_line.next().expect("Failed to get first arg");
        // trim the first character ':'
        sender = sender.trim_start_matches(':');
        // keep everything before the first '!'
        sender = sender.split('!').next().expect("Failed to get first arg");
        // get the second arg example PRIVMSG
        let command = split_line.next().expect("Failed to get second arg");
        // join the rest of the split line
        let mut message = split_line.clone().collect::<Vec<&str>>().join(" ");
        // trim the message
        message = message.trim().to_string();

        let event_type = match command {
            "464" => EventType::AuthFailed,
            "001" => EventType::AuthSuccess,
            "375" => EventType::MotdStart,
            "372" => EventType::Motd,
            "376" => EventType::MotdEnd,
            "QUIT" => EventType::Quit,
            "PRIVMSG" => {
                // trim my username plus a space and a colon
                message.drain(..receiver.len() + 2);
                // get the new first character of the message
                let first_char = message.chars().next().expect("Failed to get first char");
                // match first character of the message
                match first_char {
                    // if it's an action the message looks like this
                    // \x01ACTION is listening to [https://osu.ppy.sh/beatmapsets/57525#/173391 Igorrr - Pavor Nocturnus]\x01
                    '\x01' => {
                        // remove the first 11 characters
                        message.drain(..11);
                        // remove the last character
                        message.pop();
                        // get the first word of the message
                        let action = message.split(' ').next().expect("Failed to get first arg");
                        // get the beatmap URL located after after the first [ and up until a space character
                        let url = message
                            .split('[')
                            .nth(1)
                            .expect("Failed to get second arg")
                            .split(' ')
                            .next()
                            .expect("Failed to get first arg");
                        // match the action
                        match action {
                            "listening" => {}
                            "playing" => {}
                            "watching" => {}
                            "editing" => {}
                            _ => println!("UNKNOWN ACTION '{}' FROM MESSAGE: '{}'", action, line),
                        }
                        message = calcul_performance(url).await;
                    }
                    _ => println!("UNKNOWN MESSAGE '{}' FROM MESSAGE: '{}'", message, line),
                }
                EventType::PrivMsg
            }
            _ => {
                message = line;
                EventType::Error
            }
        };
        // return the event
        BanchoEvent {
            event_type,
            sender: sender.to_owned(),
            receiver: receiver.to_owned(),
            message,
        }
    }

    /**
     * Send a message to the bancho server
     * @param message The message to send
     */
    pub async fn send_bancho(&mut self, message: String) {
        // send using reader
        let response = self.reader.write_all(message.as_bytes()).await;
        // check if the response is an error
        match response {
            Ok(_) => (),
            Err(e) => println!("Error sending message: {}", e),
        }
    }
}

async fn init_stream() -> TcpStream {
    const IP_LIST: [&str; 2] = ["irc.ppy.sh", "cho.ppy.sh"];
    const PORT: u16 = 6667;
    const RETRY_INTERVAL_MS: u64 = 5000;
    // create the stream
    let mut stream: Option<TcpStream> = None;
    // loop until the stream is successful
    while stream.is_none() {
        for ip in IP_LIST {
            let address = format!("{}:{}", ip, PORT);
            println!("Connecting to {}", address);
            match TcpStream::connect(address).await {
                Ok(s) => {
                    println!("Connection established with {}:{}", ip, PORT);
                    stream = Some(s);
                    break;
                }
                Err(_) => {
                    println!("Connection failed, retrying in {}ms", RETRY_INTERVAL_MS);
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_INTERVAL_MS)).await;
                    continue;
                }
            }
        }
    }
    // unwrap the stream
    let stream = stream.expect("UNREACHABLE ERROR, PLEASE REPORT THIS TO THE DEVELOPER");
    stream
}

async fn calcul_performance(url: &str) -> String {
    let beatmap_set_id = url
        .split('#')
        .next()
        .expect("Failed to get first arg")
        .split('/')
        .last()
        .expect("Failed to get last arg");
    let beatmap_id = url
        .split('#')
        .last()
        .expect("Failed to get last arg")
        .split('/')
        .last()
        .expect("Failed to get last arg");
    // download the map
    let file_name = download_map(beatmap_id.parse().expect("Failed to parse beatmap_id")).await;
    // open the file
    let file = match tokio::fs::File::open(format!("maps/{}", file_name)).await {
        Ok(file) => file,
        Err(why) => panic!("Could not open file: {}", why),
    };
    // parse the map asynchronously
    let map = match Beatmap::parse(file).await {
        Ok(map) => map,
        Err(why) => panic!("Error while parsing map: {}", why),
    };
    // accuracy list of 95%, 97%, 98%, 99%, 100%
    let acc = [95.0, 97.0, 98.0, 99.0, 100.0];
    let mut pp = [0.0, 0.0, 0.0, 0.0, 0.0];
    // calculate pp for each acc
    for (i, acc) in acc.iter().enumerate() {
        pp[i] = map.pp().accuracy(*acc).calculate().await.pp();
    }
    // create a string with the pp values
    let mut result = format!(
        "[https://osu.ppy.sh/beatmapsets/{}#/{} Map] ",
        beatmap_set_id, beatmap_id
    );
    for (i, pp) in pp.iter().enumerate() {
        result.push_str(&format!("{}%: {}pp | ", acc[i], pp.round()));
    }
    // remove the extra separator symbol
    result.pop();
    // return the string
    result
}

// function that takes an ID and downloads a file from a url
async fn download_map(beatmap_id: i32) -> String {
    let url = format!("https://osu.ppy.sh/osu/{}", beatmap_id);
    // use reqwest to get the file
    let response = reqwest::get(&url).await.unwrap();
    // get the file name from the response
    let filename = response
        .url()
        .path_segments()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    // make sure a folder called 'maps' exists, if not create it
    std::fs::create_dir_all("maps").expect("Failed to create directory");
    // create a file with the same name in a folder called 'maps'
    // TODO implement a long term data storage
    let mut file =
        std::fs::File::create(format!("maps/{}", filename)).expect("Failed to create file");
    // write the response to the file
    file.write_all(&response.bytes().await.expect("Failed to read bytes"))
        .expect("Failed to write file");
    // return the file name
    filename
}