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

type ResizeRequest = tokio::task::JoinHandle<((u16, u16), Result<Value>)>;

struct RawTerminal;
impl Drop for RawTerminal {
    fn drop(&mut self) {
        // A remote TUI may have enabled these modes before disconnecting.
        // Restore the local terminal even if its final escape sequences never
        // arrive. End synchronized output first so cleanup becomes visible.
        let mut out = io::stdout();
        let _ = out.write_all(
            concat!(
                "\x1b[?2026l",
                "\x1b[?1000l",
                "\x1b[?1002l",
                "\x1b[?1003l",
                "\x1b[?1005l",
                "\x1b[?1006l",
                "\x1b[?1015l",
                "\x1b[?1004l",
                "\x1b[?2004l",
                "\x1b[>4;0m",
                "\x1b[<u",
                "\x1b[?1049l",
                "\x1b[0m",
                "\x1b[?25h"
            )
            .as_bytes(),
        );
        let _ = out.flush();
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
        let (mut socket,_)=tokio::time::timeout(Duration::from_secs(10),tokio_tungstenite::connect_async_with_config(request,Some(config),true)).await??;
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
        let mut pending_resize: Option<ResizeRequest>=None;
        let mut resize_after=tokio::time::Instant::now();
        let mut resize=tokio::time::interval(Duration::from_millis(250));
        let mut ping=tokio::time::interval(Duration::from_secs(20));
        let mut cursor=seq;
        let mut last_frame=tokio::time::Instant::now();
        loop {
            tokio::select! {
                data=rx.recv()=>{
                    let Some(data)=data else {return Ok(0);};
                    let escape=data.iter().position(|b| *b==0x1d);
                    let end=escape.unwrap_or(data.len());
                    if end>0 && !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Binary(data[..end].to_vec().into()))).await, Ok(Ok(()))) {
                            eprint!("\r\nahvm: connection lost during input; last input was not replayed\r\n");
                            let Some(next)=reconnect(api,id,sid,cursor,&mut rx).await? else {return Ok(0);};
                            socket=next;size=None;last_frame=tokio::time::Instant::now();
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
                        let next_seq=v["next_seq"].as_u64().ok_or("missing next_seq")?;
                        io::stdout().write_all(&decoded(&v)?)?; io::stdout().flush()?;
                        cursor=next_seq;last_frame=tokio::time::Instant::now();
                        if v["truncated"]==true {eprint!("\r\nahvm: session scrollback truncated\r\n");}
                        if v["eof"]==true {return exit_code(&v);}
                    },
                    Some(Ok(Message::Ping(_)))=>{
                        last_frame=tokio::time::Instant::now();
                        if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.flush()).await, Ok(Ok(()))) {
                            let Some(next)=reconnect(api,id,sid,cursor,&mut rx).await? else {return Ok(0);};
                            socket=next;size=None;last_frame=tokio::time::Instant::now();
                        }
                    },
                    Some(Ok(Message::Pong(_)))=>{last_frame=tokio::time::Instant::now();},
                    Some(Err(_))|Some(Ok(Message::Close(_)))|None=>{
                        let Some(next)=reconnect(api,id,sid,cursor,&mut rx).await? else {return Ok(0);};
                        socket=next;size=None;last_frame=tokio::time::Instant::now();
                    },
                    _=>return Err("unexpected session WebSocket frame".into()),
                },
                _=ping.tick()=>{
                    if last_frame.elapsed()>Duration::from_secs(60) || !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) {
                        let Some(next)=reconnect(api,id,sid,cursor,&mut rx).await? else {return Ok(0);};
                        socket=next;size=None;last_frame=tokio::time::Instant::now();
                    }
                },
                result=async {pending_resize.as_mut().unwrap().await}, if pending_resize.is_some()=>{
                    pending_resize=None;
                    if let Ok((now,Ok(_)))=result {size=Some(now);}
                    else {resize_after=tokio::time::Instant::now()+Duration::from_secs(2);}
                },
                _=resize.tick()=>{
                    // Geometry is best-effort. Keep at most one bounded request
                    // in flight, without blocking input/output or disconnecting
                    // the WebSocket when this separate HTTP request fails.
                    if pending_resize.is_none() && tokio::time::Instant::now()>=resize_after {
                        if let Ok(now)=crossterm::terminal::size() {
                            if now.0>0 && now.1>0 && size!=Some(now) {
                                let api=api.clone(); let id=id.to_owned();let sid=sid.to_owned();
                                pending_resize=Some(tokio::task::spawn_blocking(move || (now,api.call_with_timeout(
                                    Method::POST,&["sandboxes",&id,"sessions",&sid,"resize"],&[],
                                    Some(json!({"cols":now.0,"rows":now.1})),Some(Duration::from_secs(2))))));
                            }
                        }
                    }
                },
            }
        }
    })
}

// Preserve the PTY and its output cursor across transport failures. Input has
// no delivery acknowledgements, so never replay possibly executed keystrokes.
type SessionSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn reconnect(
    api: &Api,
    id: &str,
    sid: &str,
    cursor: u64,
    rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<Option<SessionSocket>> {
    eprint!("\r\nahvm: reconnecting to existing session; input paused (Ctrl-] detaches)\r\n");
    loop {
        let auth = api.clone();
        let auth = tokio::task::spawn_blocking(move || auth.renew_stream_auth()).await??;
        let request = auth.stream_request(id, sid, cursor)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(4 * 1024 * 1024))
            .max_frame_size(Some(4 * 1024 * 1024));
        let connect = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(request, Some(config), true),
        );
        tokio::pin!(connect);
        loop {
            tokio::select! {
                result=&mut connect=>{
                    match result {
                        Ok(Ok((socket,_)))=>{eprint!("\r\nahvm: session reconnected\r\n");return Ok(Some(socket));},
                        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response)))
                            if matches!(response.status().as_u16(),401|403|404|410|422)=>{
                                return Err(format!("session reconnect refused: HTTP {}; resume cursor {cursor}",response.status()).into());
                            },
                        _=>break,
                    }
                },
                data=rx.recv()=>{
                    if data.is_none_or(|d|d.is_empty()||d.contains(&0x1d)){return Ok(None);}
                }
            }
        }
        let delay = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _=&mut delay=>break,
                data=rx.recv()=>{if data.is_none_or(|d|d.is_empty()||d.contains(&0x1d)){return Ok(None);}}
            }
        }
    }
}
