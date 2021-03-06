use std::io::{self, BufRead, BufReader, Cursor, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use std::{fmt, thread};

use byteorder::{LittleEndian, WriteBytesExt};
use crate::error::Error;
use crate::station::{Position, Station};
use crate::tts::text_to_speech;
use crate::worker::{Context, Worker};
use ogg::reading::PacketReader;
use uuid::Uuid;

const MAX_FRAME_LENGTH: usize = 1024;

pub struct AtisSrsClient {
    sguid: String,
    station: Station,
    worker: Vec<Worker<()>>,
}

impl AtisSrsClient {
    pub fn new(station: Station) -> Self {
        let sguid = Uuid::new_v4();
        let sguid = base64::encode_config(sguid.as_bytes(), base64::URL_SAFE_NO_PAD);
        assert_eq!(sguid.len(), 22);

        AtisSrsClient {
            sguid,
            station,
            worker: Vec::new(),
        }
    }

    pub fn start(&mut self) -> Result<(), Error> {
        if self.worker.len() > 0 {
            // already started
            return Ok(());
        }

        let mut stream = TcpStream::connect("127.0.0.1:5002")?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;

        let sync_msg = Message {
            client: Some(Client {
                client_guid: &self.sguid,
                name: &self.station.name,
                position: self.station.airfield.position.clone(),
                coalition: Coalition::Blue,
            }),
            msg_type: MsgType::Sync,
            version: "1.5.3.5",
        };

        serde_json::to_writer(&stream, &sync_msg)?;
        stream.write_all(&['\n' as u8])?;

        let mut rd = BufReader::new(stream.try_clone().unwrap()); // TODO: unwrap?

        // spawn audio broadcast thread
        let sguid = self.sguid.clone();
        let station = self.station.clone();
        self.worker.push(Worker::new(move |ctx| {
            if let Err(err) = audio_broadcast(ctx, sguid, station) {
                error!("Error starting SRS broadcast: {}", err);
            }
        }));

        // spawn thread that sends an update RPC call to SRS every ~5 seconds
        let sguid = self.sguid.clone();
        let name = self.station.name.clone();
        let position = self.station.airfield.position.clone();
        self.worker.push(Worker::new(move |ctx| {
            let mut send_update = || -> Result<(), Error> {
                // send update
                let upd_msg = Message {
                    client: Some(Client {
                        client_guid: &sguid,
                        name: &name,
                        position: position.clone(),
                        coalition: Coalition::Blue,
                    }),
                    msg_type: MsgType::Update,
                    version: "1.5.3.5",
                };

                serde_json::to_writer(&mut stream, &upd_msg)?;
                stream.write_all(&['\n' as u8])?;

                Ok(())
            };

            loop {
                if let Err(err) = send_update() {
                    error!("Error sending update to SRS: {}", err);
                }

                debug!("SRS Update sent");

                if ctx.should_stop_timeout(Duration::from_secs(5)) {
                    return ();
                }
            }
        }));

        self.worker.push(Worker::new(move |ctx| {
            let mut data = Vec::new();

            loop {
                match rd.read_until(b'\n', &mut data) {
                    Ok(bytes_read) => {
                        if bytes_read == 0 {
                            return ();
                        }

                        data.clear();
                        // ignore received messages ...
                    }
                    Err(err) => match err.kind() {
                        io::ErrorKind::TimedOut => {}
                        _ => {
                            error!(
                                "Error ({:?}) receiving update from SRS: {}",
                                err.kind(),
                                err
                            );
                        }
                    },
                }

                if ctx.should_stop() {
                    return ();
                }
            }
        }));

        // TODO: endless loop required?

        Ok(())
    }

    pub fn stop(self) {
        for worker in self.worker.into_iter() {
            worker.stop();
        }
    }

    pub fn pause(&self) {
        for worker in &self.worker {
            worker.pause();
        }
    }

    pub fn unpause(&self) {
        for worker in &self.worker {
            worker.unpause();
        }
    }
}

fn audio_broadcast(ctx: Context, sguid: String, station: Station) -> Result<(), Error> {
    let interval = Duration::from_secs(60 * 60); // 60min
    let mut interval_start;
    let mut report_ix = 0;
    loop {
        interval_start = Instant::now();

        // TODO: unwrap
        let report = station.generate_report(report_ix)?;
        report_ix += 1;
        info!("Report: {}", report);

        let data = text_to_speech(&report)?;
        let mut data = Cursor::new(data);

        let mut stream = TcpStream::connect("127.0.0.1:5003")?;
        stream.set_nodelay(true)?;

        loop {
            let elapsed = Instant::now() - interval_start;
            if elapsed > interval {
                // every 60min, generate a new report
                break;
            }

            data.set_position(0);
            let start = Instant::now();
            let mut size = 0;
            let mut audio = PacketReader::new(data);
            let mut id: u64 = 1;
            while let Some(pck) = audio.read_packet()? {
                let pck_size = pck.data.len();
                if pck_size == 0 {
                    continue;
                }
                size += pck_size;
                let frame = pack_frame(&sguid, id, station.atis_freq, &pck.data)?;
                stream.write(&frame)?;
                id += 1;

                // 32 kBit/s
                let secs = (size * 8) as f64 / 1024.0 / 32.0;

                let playtime = Duration::from_millis((secs * 1000.0) as u64);
                let elapsed = Instant::now() - start;
                if playtime > elapsed {
                    thread::sleep(playtime - elapsed);
                }

                if ctx.should_stop() {
                    return Ok(());
                }
            }

            info!("TOTAL SIZE: {}", size);

            // 32 kBit/s
            let secs = (size * 8) as f64 / 1024.0 / 32.0;
            info!("SECONDS: {}", secs);

            let playtime = Duration::from_millis((secs * 1000.0) as u64);
            let elapsed = Instant::now() - start;
            if playtime > elapsed {
                thread::sleep(playtime - elapsed);
            }

            if ctx.should_stop_timeout(Duration::from_secs(3)) {
                return Ok(());
            }

            data = audio.into_inner();
        }
    }

    //    Ok(())
}

fn pack_frame(sguid: &str, id: u64, freq: u64, rd: &Vec<u8>) -> Result<Vec<u8>, io::Error> {
    let mut frame = Cursor::new(Vec::with_capacity(MAX_FRAME_LENGTH));

    // header segment will be written at the end
    frame.set_position(6);

    // - AUDIO SEGMENT
    let len_before = frame.position();
    // AudioPart1
    frame.write_all(&rd)?;
    let len_audio_part = frame.position() - len_before;

    // - FREQUENCY SEGMENT
    let len_before = frame.position();
    // Frequency
    frame.write_f64::<LittleEndian>(freq as f64)?;
    // Modulation
    //    AM = 0,
    //    FM = 1,
    //    INTERCOM = 2,
    //    DISABLED = 3
    frame.write_all(&[0])?;
    // Encryption
    //    NO_ENCRYPTION = 0,
    //    ENCRYPTION_JUST_OVERLAY = 1,
    //    ENCRYPTION_FULL = 2,
    //    ENCRYPTION_COCKPIT_TOGGLE_OVERLAY_CODE = 3
    frame.write_all(&[0])?;
    let len_frequency = frame.position() - len_before;

    // - FIXED SEGMENT
    // UnitId
    frame.write_u32::<LittleEndian>(0)?;
    // PacketId
    frame.write_u64::<LittleEndian>(id)?;
    // GUID
    frame.write_all(sguid.as_bytes())?;

    // - HEADER SEGMENT
    let len_packet = frame.get_ref().len();
    frame.set_position(0);
    // Packet Length
    frame.write_u16::<LittleEndian>(len_packet as u16)?;
    // AudioPart1 Length
    frame.write_u16::<LittleEndian>(len_audio_part as u16)?;
    // FrequencyPart Length
    frame.write_u16::<LittleEndian>(len_frequency as u16)?;

    Ok(frame.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MsgType {
    Update,
    Sync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Coalition {
    Blue,
    Red,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct Client<'a> {
    client_guid: &'a str,
    name: &'a str,
    position: Position,
    coalition: Coalition,
    // RadioInfo
    // ClientChannelId
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct Message<'a> {
    client: Option<Client<'a>>,
    msg_type: MsgType,
    // Clients
    // ServerSettings
    // ExternalAWACSModePassword
    version: &'a str,
}

impl ::serde::Serialize for MsgType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ::serde::Serializer,
    {
        // Serialize the enum as a u64.
        serializer.serialize_u64(match *self {
            MsgType::Update => 1,
            MsgType::Sync => 2,
        })
    }
}

impl<'de> ::serde::Deserialize<'de> for MsgType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: ::serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> ::serde::de::Visitor<'de> for Visitor {
            type Value = MsgType;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("positive integer")
            }

            fn visit_u64<E>(self, value: u64) -> Result<MsgType, E>
            where
                E: ::serde::de::Error,
            {
                // Rust does not come with a simple way of converting a
                // number to an enum, so use a big `match`.
                match value {
                    1 => Ok(MsgType::Update),
                    2 => Ok(MsgType::Sync),
                    _ => Err(E::custom(format!(
                        "unknown {} value: {}",
                        stringify!(MsgType),
                        value
                    ))),
                }
            }
        }

        // Deserialize the enum from a u64.
        deserializer.deserialize_u64(Visitor)
    }
}

impl ::serde::Serialize for Coalition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ::serde::Serializer,
    {
        // Serialize the enum as a u64.
        serializer.serialize_u64(match *self {
            Coalition::Blue => 2,
            Coalition::Red => 1,
        })
    }
}

impl<'de> ::serde::Deserialize<'de> for Coalition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: ::serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> ::serde::de::Visitor<'de> for Visitor {
            type Value = Coalition;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("positive integer")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Coalition, E>
            where
                E: ::serde::de::Error,
            {
                // Rust does not come with a simple way of converting a
                // number to an enum, so use a big `match`.
                match value {
                    1 => Ok(Coalition::Red),
                    2 => Ok(Coalition::Blue),
                    _ => Err(E::custom(format!(
                        "unknown {} value: {}",
                        stringify!(Coalition),
                        value
                    ))),
                }
            }
        }

        // Deserialize the enum from a u64.
        deserializer.deserialize_u64(Visitor)
    }
}
