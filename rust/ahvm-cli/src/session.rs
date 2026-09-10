use crate::{
    client::{exit_code, Api},
    commands::decoded,
    Result,
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{json, Value};
use std::{
    io::{self, IsTerminal, Read, Write},
    time::Duration,
};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

struct RawTerminal;
impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
pub fn require_terminal() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("interactive attach needs a terminal; use session input/read for pipes".into());
    }
    Ok(())
}
pub fn attach(api: &Api, id: &str, sid: &str, seq: u64) -> Result<i32> {
    require_terminal()?;
    let request = api.stream_request(id, sid, seq)?;
    crossterm::terminal::enable_raw_mode()?;
    let _raw = RawTerminal;
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let config=WebSocketConfig::default().max_message_size(Some(4*1024*1024)).max_frame_size(Some(4*1024*1024));
        let (mut socket,_)=tokio::time::timeout(Duration::from_secs(10),tokio_tungstenite::connect_async_with_config(request,Some(config),false)).await??;
        let (tx,mut rx)=tokio::sync::mpsc::channel(8);
        // Raw stdin retains escape sequences and paste bytes. This bounded
        // thread never owns terminal cleanup; process exit releases its read.
        std::thread::spawn(move || {
            let mut input=io::stdin().lock(); let mut buf=[0;4096];
            loop {
                match input.read(&mut buf) {
                    Ok(0)|Err(_)=>{let _=tx.blocking_send(Vec::new());break;},
                    Ok(n)=>if tx.blocking_send(buf[..n].to_vec()).is_err() {break;},
                }
            }
        });
        let mut size=None;
        let mut resize=tokio::time::interval(Duration::from_millis(250));
        let mut ping=tokio::time::interval(Duration::from_secs(20));
        let mut cursor=seq;
        loop {
            tokio::select! {
                data=rx.recv()=>{
                    let Some(data)=data else {return Ok(0);};
                    let escape=data.iter().position(|b| *b==0x1d);
                    let end=escape.unwrap_or(data.len());
                    if end>0 {
                        tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Binary(data[..end].to_vec().into()))).await??;
                    }
                    if data.is_empty() || escape.is_some() {
                        let _=tokio::time::timeout(Duration::from_secs(1),socket.close(None)).await;
                        return Ok(0);
                    }
                },
                message=socket.next()=>match message {
                    Some(Ok(Message::Text(text)))=>{
                        let v:Value=serde_json::from_str(&text)?;
                        if let Some(error)=v["error"].as_str() {return Err(format!("session: {error}; resume cursor {cursor}").into());}
                        cursor=v["next_seq"].as_u64().ok_or("missing next_seq")?;
                        io::stdout().write_all(&decoded(&v)?)?; io::stdout().flush()?;
                        if v["truncated"]==true {eprint!("\r\nahvm: session scrollback truncated\r\n");}
                        if v["eof"]==true {return exit_code(&v);}
                    },
                    Some(Ok(Message::Ping(_)))=>{tokio::time::timeout(Duration::from_secs(5),socket.flush()).await??;},
                    Some(Ok(Message::Pong(_)))=>{},
                    Some(Err(e))=>return Err(format!("session disconnected: {e}; resume cursor {cursor}").into()),
                    Some(Ok(Message::Close(_)))|None=>return Err(format!("session disconnected before EOF; resume cursor {cursor}").into()),
                    _=>return Err("unexpected session WebSocket frame".into()),
                },
                _=ping.tick()=>{tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Ping(Vec::new().into()))).await??;},
                _=resize.tick()=>{
                    if let Ok(now)=crossterm::terminal::size() {
                        if size!=Some(now) {
                            let api=api.clone(); let id=id.to_owned();let sid=sid.to_owned();
                            tokio::task::spawn_blocking(move || api.call(Method::POST,&["sandboxes",&id,"sessions",&sid,"resize"],&[],Some(json!({"cols":now.0,"rows":now.1})))).await??;
                            size=Some(now);
                        }
                    }
                },
            }
        }
    })
}
