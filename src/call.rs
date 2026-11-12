//! Video call input: a remote guest opens a link in an ordinary browser and
//! sends camera and microphone over WebRTC. The link's page and the exchange
//! that sets the call up are served by the remote control port; the call is
//! received by the windowless browser that draws web inputs, so the guest's
//! picture arrives like a web page's. Their sound is handed over in blocks
//! through the browser's debugging connection, and the program goes back to
//! them the same way, with the sound they asked for.

use crate::api::HTTP_PORT;
use crate::browser::{self, PageHook, Socket};
use crate::engine::Engine;
use crate::frame::Frame;
use crate::model::{Kind, ReturnSound};
use crate::secret::base64;
use crate::sources::{InputRt, Ring};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Width of the program picture sent back: enough to follow the show, light
/// enough for a guest on a weak connection.
const RETURN_W: usize = 640;
const RETURN_FPS: f32 = 15.0;
/// A guest's offer is a few kilobytes; far more is not a browser talking.
pub const MAX_OFFER: usize = 64 * 1024;
type Answer = mpsc::Sender<Result<String, String>>;

/// What passes between a running call and the rest of the desk.
pub struct CallLink {
    offers: Mutex<Vec<(String, Answer)>>,
    program: Mutex<Option<Arc<Frame>>>,
    program_seq: AtomicU64,
    /// Sound going back to the guest, filled by the sound thread.
    pub sound: Ring,
    connected: AtomicBool,
}

impl Default for CallLink {
    fn default() -> Self {
        Self {
            offers: Mutex::new(Vec::new()),
            program: Mutex::new(None),
            program_seq: AtomicU64::new(0),
            sound: Ring::new(0.5),
            connected: AtomicBool::new(false),
        }
    }
}

impl CallLink {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Hands a guest's offer to the page receiving the call and waits for its
    /// answer, which holds everything the guest's browser needs to connect.
    pub fn answer(&self, offer: String) -> Result<String, String> {
        let (tx, rx) = mpsc::channel();
        self.offers.lock().unwrap().push((offer, tx));
        rx.recv_timeout(Duration::from_secs(20)).map_err(|_| t!("call.no_answer").to_string())?
    }

    fn offer_program(&self, seq: u64, frame: impl FnOnce() -> Arc<Frame>) {
        if self.program_seq.swap(seq, Ordering::Relaxed) != seq {
            *self.program.lock().unwrap() = Some(frame());
        }
    }
}

/// A fresh call input, with a link nobody has seen yet.
pub fn new_call() -> Kind {
    Kind::VideoCall {
        token: new_token(),
        address: String::new(),
        stun: String::new(),
        turn: String::new(),
        turn_user: String::new(),
        turn_password: String::new(),
        return_sound: ReturnSound::default(),
    }
}

/// 128 random bits from the system: the link is the guest's only key.
pub fn new_token() -> String {
    let mut bytes = [0u8; 16];
    if rustls::crypto::ring::default_provider().secure_random.fill(&mut bytes).is_err() {
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compares without stopping at the first difference, so the time taken
/// says nothing about how much of a guessed link was right.
fn same_token(given: &str, real: &str) -> bool {
    !real.is_empty()
        && given.len() == real.len()
        && given.bytes().zip(real.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

/// The address this computer has on the network, as a guest would reach it.
pub fn lan_address() -> Option<String> {
    // Connecting a UDP socket sends nothing: it only asks the system which of
    // its addresses would carry traffic outwards.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

/// The link to send the guest. `address` may be a name, a name and port, or
/// a whole address in front of a proxy; empty uses this computer's.
pub fn guest_link(token: &str, address: &str, lan: bool) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    let address = address.trim().trim_end_matches('/');
    let base = if address.contains("://") {
        address.to_string()
    } else if !address.is_empty() {
        let port = if address.contains(':') { String::new() } else { format!(":{HTTP_PORT}") };
        format!("https://{address}{port}")
    } else {
        let host = if lan { lan_address() } else { None };
        format!("https://{}:{HTTP_PORT}", host.as_deref().unwrap_or("127.0.0.1"))
    };
    Some(format!("{base}/call/{token}"))
}

/// The STUN and TURN servers both ends use, in the form browsers expect.
/// Several addresses may be given, separated by spaces or commas.
pub fn ice_servers(stun: &str, turn: &str, user: &str, password: &str) -> String {
    let urls = |s: &str| s.split([' ', ',']).filter(|u| !u.is_empty()).map(String::from).collect::<Vec<_>>();
    let mut list = Vec::new();
    list.extend(urls(stun).into_iter().map(|u| json!({ "urls": u })));
    list.extend(
        urls(turn).into_iter().map(|u| json!({ "urls": u, "username": user, "credential": password })),
    );
    // Inside a script element, `</script>` in a value would end it.
    Value::Array(list).to_string().replace('<', "\\u003c")
}

fn return_size(w: usize, h: usize) -> (usize, usize) {
    let rh = (RETURN_W * h / w.max(1)) & !1;
    (RETURN_W, rh.max(2))
}

fn jpeg(frame: &Frame, w: usize, h: usize) -> Option<Vec<u8>> {
    let small = frame.thumbnail(w, h);
    let rgb: Vec<u8> = small.data.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70)
        .encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(out)
}

pub fn run_call(rt: &Arc<InputRt>, ice: &str, w: usize, h: usize, fps: u32) {
    let Some(link) = rt.call.clone() else { return };
    let (rw, rh) = return_size(w, h);
    let page = receiver_html(ice, rw, rh);
    let url = format!("data:text/html;base64,{}", base64(page.as_bytes()));
    let mut hook = CallPage {
        rt: rt.clone(),
        link: link.clone(),
        answers: HashMap::new(),
        size: (rw, rh),
        sent_seq: 0,
        last_picture: Instant::now(),
        last_sound: Instant::now(),
        pcm: Vec::new(),
    };
    browser::run_page(rt, &url, false, (w, h), fps, &mut hook);
    link.set_connected(false);
}

/// Talks to the page receiving the call.
struct CallPage {
    rt: Arc<InputRt>,
    link: Arc<CallLink>,
    answers: HashMap<u64, Answer>,
    size: (usize, usize),
    sent_seq: u64,
    last_picture: Instant,
    last_sound: Instant,
    pcm: Vec<f32>,
}

impl CallPage {
    fn evaluate(ws: &mut Socket, id: &mut u64, expression: String, wait: bool) -> Option<u64> {
        *id += 1;
        let params = json!({ "expression": expression, "awaitPromise": wait, "returnByValue": wait });
        browser::send(ws, *id, "Runtime.evaluate", params).then_some(*id)
    }

    fn send_picture(&mut self, ws: &mut Socket, id: &mut u64) {
        if self.last_picture.elapsed() < Duration::from_secs_f32(1.0 / RETURN_FPS) {
            return;
        }
        let seq = self.link.program_seq.load(Ordering::Relaxed);
        if seq == self.sent_seq {
            return;
        }
        let Some(frame) = self.link.program.lock().unwrap().clone() else { return };
        self.sent_seq = seq;
        self.last_picture = Instant::now();
        if let Some(data) = jpeg(&frame, self.size.0, self.size.1) {
            Self::evaluate(ws, id, format!("lcFrame('{}')", base64(&data)), false);
        }
    }

    fn send_sound(&mut self, ws: &mut Socket, id: &mut u64) {
        if self.last_sound.elapsed() < Duration::from_millis(40) {
            return;
        }
        self.last_sound = Instant::now();
        let n = self.link.sound.len() & !1;
        if n == 0 {
            return;
        }
        self.pcm.resize(n, 0.0);
        self.link.sound.pull(&mut self.pcm);
        let bytes: Vec<u8> =
            self.pcm.iter().flat_map(|v| ((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
        Self::evaluate(ws, id, format!("lcSound('{}')", base64(&bytes)), false);
    }

    fn hear(&self, payload: &str) {
        let Some(ring) = &self.rt.audio else { return };
        let bytes = crate::api::base64_decode(payload);
        let samples: Vec<f32> =
            bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0).collect();
        ring.push(&samples);
    }

    fn state(&self, state: &str) {
        self.link.set_connected(state == "connected");
        match state {
            "connected" | "new" | "connecting" => self.rt.clear_status(),
            _ => self.rt.set_status(t!("call.guest_disconnected")),
        }
    }
}

impl PageHook for CallPage {
    fn prepare(&mut self, ws: &mut Socket, id: &mut u64) -> bool {
        // A page lost and opened again has forgotten what it was asked.
        self.answers.clear();
        self.link.set_connected(false);
        let mut ok = true;
        for (method, params) in [
            ("Runtime.enable", json!({})),
            ("Runtime.addBinding", json!({ "name": "lcAudio" })),
            ("Runtime.addBinding", json!({ "name": "lcState" })),
        ] {
            *id += 1;
            ok &= browser::send(ws, *id, method, params);
        }
        ok
    }

    fn turn(&mut self, ws: &mut Socket, id: &mut u64) {
        let offers: Vec<(String, Answer)> = std::mem::take(&mut *self.link.offers.lock().unwrap());
        for (offer, tx) in offers {
            match Self::evaluate(ws, id, format!("lcAnswer({})", Value::String(offer)), true) {
                Some(n) => {
                    self.answers.insert(n, tx);
                }
                None => {
                    let _ = tx.send(Err(t!("call.browser_unavailable").into()));
                }
            }
        }
        if self.link.is_connected() {
            self.send_picture(ws, id);
            self.send_sound(ws, id);
        }
    }

    fn message(&mut self, v: &Value) {
        if v["method"] == "Runtime.bindingCalled" {
            let payload = v["params"]["payload"].as_str().unwrap_or("");
            match v["params"]["name"].as_str() {
                Some("lcAudio") => self.hear(payload),
                Some("lcState") => self.state(payload),
                _ => {}
            }
        } else if let Some(tx) = v["id"].as_u64().and_then(|n| self.answers.remove(&n)) {
            let sdp = v["result"]["result"]["value"].as_str().map(String::from);
            let _ = tx.send(sdp.ok_or_else(|| t!("call.refused").to_string()));
        }
    }
}

impl Engine {
    /// The running call a guest link opens, with its STUN and TURN servers.
    fn call_for(&self, token: &str) -> Option<(Arc<CallLink>, String)> {
        let (id, ice) = {
            let s = self.state.lock().unwrap();
            s.project.inputs.iter().find_map(|i| match &i.kind {
                Kind::VideoCall { token: real, stun, turn, turn_user, turn_password, .. }
                    if same_token(token, real) =>
                {
                    Some((i.id, ice_servers(stun, turn, turn_user, turn_password)))
                }
                _ => None,
            })?
        };
        Some((self.rt(id)?.call.clone()?, ice))
    }

    pub fn guest_page(&self, token: &str) -> Option<String> {
        let (_, ice) = self.call_for(token)?;
        Some(guest_html(&ice))
    }

    /// None when no input has this link.
    pub fn call_answer(&self, token: &str, offer: String) -> Option<Result<String, String>> {
        let (link, _) = self.call_for(token)?;
        Some(link.answer(offer))
    }

    /// Hands the program to the calls with a guest, who see it as it changes.
    pub(crate) fn show_program_to_guests(&self, rts: &HashMap<u64, Arc<InputRt>>) {
        let seq = self.program_seq.load(Ordering::Acquire);
        for link in rts.values().filter_map(|r| r.call.as_ref()).filter(|l| l.is_connected()) {
            link.offer_program(seq, || self.program.lock().unwrap().clone());
        }
    }
}

/// Texts a page's script shows, as a script value that cannot end the
/// script element it sits in.
fn script_text(words: &[(&str, &str)]) -> String {
    let map = words.iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect();
    Value::Object(map).to_string().replace('<', "\\u003c")
}

/// The page receiving the call, in the desk's language: its words are drawn
/// on the input's picture.
fn receiver_html(ice: &str, rw: usize, rh: usize) -> String {
    let words = [
        ("waiting", t!("call.receiver.waiting")),
        ("connecting", t!("call.receiver.connecting")),
        ("disconnected", t!("call.guest_disconnected")),
    ];
    // The operator's servers go in last, so nothing typed there is read
    // again as a slot.
    RECEIVER_PAGE
        .replace("__TEXT__", &script_text(&words))
        .replace("__RW__", &rw.to_string())
        .replace("__RH__", &rh.to_string())
        .replace("__ICE__", ice)
}

/// The guest's page, in the desk's language.
fn guest_html(ice: &str) -> String {
    let page = crate::api::localize(
        GUEST_PAGE,
        &[
            ("title", t!("call.guest.title")),
            ("intro", t!("call.guest.intro")),
            ("join", t!("call.guest.join")),
            ("mute_mic", t!("call.guest.mute_mic")),
            ("mute_camera", t!("call.guest.mute_camera")),
        ],
    );
    let words = [
        ("mute_mic", t!("call.guest.mute_mic")),
        ("unmute_mic", t!("call.guest.unmute_mic")),
        ("mute_camera", t!("call.guest.mute_camera")),
        ("unmute_camera", t!("call.guest.unmute_camera")),
        ("needs_https", t!("call.guest.needs_https")),
        ("refused", t!("call.guest.refused")),
        ("retry", t!("call.guest.retry")),
        ("connecting", t!("call.guest.connecting")),
        ("connected", t!("call.guest.connected")),
        ("lost", t!("call.guest.lost")),
        ("reconnect", t!("call.guest.reconnect")),
        ("no_answer", t!("call.guest.no_answer")),
    ];
    page.replace("__TEXT__", &script_text(&words)).replace("__ICE__", ice)
}

/// Runs in the windowless browser. What it draws is the input's picture.
const RECEIVER_PAGE: &str = r#"<!doctype html><html><meta charset="utf-8"><style>
html,body{margin:0;height:100%;background:#000;overflow:hidden}
video{position:absolute;inset:0;width:100%;height:100%;object-fit:contain}
#w{position:absolute;inset:0;display:flex;align-items:center;justify-content:center;color:#ccc;font:6vh system-ui,sans-serif}
</style><video id=v autoplay playsinline muted></video><div id=w></div><script>
const T=__TEXT__,ICE=__ICE__,v=document.getElementById('v'),w=document.getElementById('w');w.textContent=T.waiting;
const c=document.createElement('canvas');c.width=__RW__;c.height=__RH__;
const g=c.getContext('2d');g.fillRect(0,0,c.width,c.height);
const ac=new AudioContext({sampleRate:48000}),out=ac.createMediaStreamDestination();
let pc=null,heard=null,queue=[],at=0,queued=0;
const gen=ac.createScriptProcessor(2048,0,2);
gen.onaudioprocess=e=>{const l=e.outputBuffer.getChannelData(0),r=e.outputBuffer.getChannelData(1);
for(let i=0;i<l.length;i++){while(queue.length&&at>=queue[0].length){queue.shift();at=0}
if(!queue.length){l[i]=r[i]=0;continue}const q=queue[0];l[i]=q[at];r[i]=q[at+1];at+=2;queued-=2}};
gen.connect(out);
const back=new MediaStream([c.captureStream(15).getVideoTracks()[0],out.stream.getAudioTracks()[0]]);
function bytes(b){const s=atob(b),a=new Uint8Array(s.length);for(let i=0;i<s.length;i++)a[i]=s.charCodeAt(i);return a}
function lcSound(b){const p=new Int16Array(bytes(b).buffer),f=new Float32Array(p.length);
for(let i=0;i<p.length;i++)f[i]=p[i]/32768;queue.push(f);queued+=f.length;
while(queued>28800&&queue.length>1){queued-=queue[0].length-at;queue.shift();at=0}}
function lcFrame(b){createImageBitmap(new Blob([bytes(b)],{type:'image/jpeg'})).then(i=>{g.drawImage(i,0,0,c.width,c.height);i.close()})}
function hear(track){const s=new MediaStream([track]);
const a=new Audio();a.muted=true;a.srcObject=s;a.play().catch(()=>{});
if(heard){heard.src.disconnect();heard.p.disconnect()}
const src=ac.createMediaStreamSource(s),p=ac.createScriptProcessor(2048,2,2);
p.onaudioprocess=e=>{const l=e.inputBuffer.getChannelData(0),r=e.inputBuffer.getChannelData(1),n=l.length,b=new Int16Array(n*2);
for(let i=0;i<n;i++){b[2*i]=Math.max(-1,Math.min(1,l[i]))*32767;b[2*i+1]=Math.max(-1,Math.min(1,r[i]))*32767}
const u=new Uint8Array(b.buffer);let t='';for(let i=0;i<u.length;i+=8192)t+=String.fromCharCode.apply(null,u.subarray(i,i+8192));
lcAudio(btoa(t))};
src.connect(p);p.connect(ac.destination);heard={src,p,a}}
function gathered(pc){return new Promise(r=>{if(pc.iceGatheringState=='complete')return r();
pc.addEventListener('icegatheringstatechange',()=>{if(pc.iceGatheringState=='complete')r()});setTimeout(r,3000)})}
async function lcAnswer(sdp){ac.resume();if(pc)pc.close();
const me=new RTCPeerConnection({iceServers:ICE});pc=me;
me.ontrack=e=>{if(e.track.kind=='video')v.srcObject=new MediaStream([e.track]);else hear(e.track)};
me.onconnectionstatechange=()=>{if(pc!==me)return;const s=me.connectionState;lcState(s);
w.style.display=s=='connected'?'none':'flex';
w.textContent=s=='new'||s=='connecting'?T.connecting:T.disconnected};
await me.setRemoteDescription({type:'offer',sdp});
for(const t of back.getTracks())me.addTrack(t,back);
await me.setLocalDescription(await me.createAnswer());
await gathered(me);return me.localDescription.sdp}
</script></html>"#;

/// What the guest opens. Served over https, without which their browser
/// keeps the camera closed.
const GUEST_PAGE: &str = r#"<!doctype html><html lang="{{lang}}"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>{{title}}</title>
<style>body{background:#131314;color:#eee;font:16px system-ui,sans-serif;margin:0;padding:12px;text-align:center}
video{background:#000;border-radius:6px;max-width:100%}#back{width:100%;max-width:960px;aspect-ratio:16/9}
#me{width:240px;transform:scaleX(-1)}
button{background:#343538;color:#fff;border:0;border-radius:4px;padding:14px 20px;margin:6px;font-size:16px}
#j{background:#0a7a16}</style>
<h2>LiveCraft</h2><p id=s>{{intro}}</p>
<div><button id=j onclick=join()>{{join}}</button><button id=m onclick="mute('audio',this)" hidden>{{mute_mic}}</button><button id=c onclick="mute('video',this)" hidden>{{mute_camera}}</button></div>
<video id=back autoplay playsinline></video><p><video id=me autoplay playsinline muted></video></p>
<script>
const T=__TEXT__,ICE=__ICE__,$=i=>document.getElementById(i);let pc=null,media=null;
function say(t){$('s').textContent=t}
async function devices(){for(const want of [{video:true,audio:true},{audio:true},{video:true}]){
try{return await navigator.mediaDevices.getUserMedia(want)}catch(e){}}return null}
function mute(kind,b){const t=media.getTracks().filter(x=>x.kind==kind);if(!t.length)return;
const on=!t[0].enabled;t.forEach(x=>x.enabled=on);b.textContent=T[(on?'mute_':'unmute_')+(kind=='audio'?'mic':'camera')]}
function gathered(pc){return new Promise(r=>{if(pc.iceGatheringState=='complete')return r();
pc.addEventListener('icegatheringstatechange',()=>{if(pc.iceGatheringState=='complete')r()});setTimeout(r,3000)})}
function again(t,label){say(t);$('j').textContent=label;$('j').hidden=false}
async function join(){$('j').hidden=true;
if(!navigator.mediaDevices){say(T.needs_https);return}
media=media||await devices();
if(!media){again(T.refused,T.retry);return}
$('me').srcObject=media;say(T.connecting);document.body.dataset.state='connecting';
if(pc)pc.close();const me=new RTCPeerConnection({iceServers:ICE});pc=me;
for(const t of media.getTracks())me.addTrack(t,media);
for(const k of ['audio','video'])if(!media.getTracks().some(t=>t.kind==k))me.addTransceiver(k,{direction:'recvonly'});
me.ontrack=e=>{if($('back').srcObject!==e.streams[0])$('back').srcObject=e.streams[0]};
me.onconnectionstatechange=()=>{if(pc!==me)return;const s=me.connectionState;document.body.dataset.state=s;
if(s=='connected'){say(T.connected);$('m').hidden=$('c').hidden=false}
if(s=='failed'||s=='closed')again(T.lost,T.reconnect)};
await me.setLocalDescription(await me.createOffer());await gathered(me);
let r=null;try{r=await fetch(location.pathname.replace(/\/+$/,'')+'/offer',{method:'POST',body:me.localDescription.sdp})}catch(e){}
if(!r||!r.ok){again(T.no_answer,T.retry);return}
await me.setRemoteDescription({type:'answer',sdp:await r.text()})}
</script></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_call_gets_its_own_unguessable_token() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn a_link_only_opens_with_the_whole_token() {
        assert!(same_token("abcd", "abcd"));
        assert!(!same_token("abc", "abcd"));
        assert!(!same_token("abce", "abcd"));
        assert!(!same_token("", ""), "an input without a token has no link");
    }

    #[test]
    fn the_link_goes_where_the_guest_can_reach() {
        assert_eq!(guest_link("t", "", false).unwrap(), "https://127.0.0.1:8088/call/t");
        assert_eq!(guest_link("t", "studio.example", true).unwrap(), "https://studio.example:8088/call/t");
        assert_eq!(guest_link("t", "studio.example:443", true).unwrap(), "https://studio.example:443/call/t");
        assert_eq!(guest_link("t", "https://x.example/", true).unwrap(), "https://x.example/call/t");
        assert_eq!(guest_link("", "", true), None);
    }

    #[test]
    fn ice_servers_cannot_break_out_of_the_page_script() {
        let list = ice_servers("stun:a:3478 stun:b:3478", "turn:t:3478", "u", "p</script>");
        let parsed: Value = serde_json::from_str(&list).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 3);
        assert_eq!(parsed[2]["credential"], "p</script>");
        assert!(!list.contains("</script>"));
        assert_eq!(ice_servers("", "", "", ""), "[]");
    }

    /// The words a page's script shows, as the page holds them.
    fn script_words(page: &str) -> Value {
        let start = page.find("const T=").unwrap() + "const T=".len();
        let end = start + page[start..].find(",ICE=").unwrap();
        serde_json::from_str(&page[start..end]).unwrap()
    }

    /// Both pages are filled from the catalog in the desk's language, and
    /// what their scripts show cannot end the script it sits in.
    #[test]
    fn the_call_pages_speak_the_desk_language() {
        use crate::i18n::{Lang, set_language};
        for lang in Lang::ALL {
            set_language(lang);
            let (guest, receiver) = (guest_html("[]"), receiver_html("[]", 640, 360));
            for page in [&guest, &receiver] {
                assert!(!page.contains("{{") && !page.contains("__"), "{lang:?}: a slot is left");
            }
            assert!(guest.contains(&format!("<html lang=\"{}\">", lang.code())));
            assert!(guest.contains(&format!(">{}</button>", t!("call.guest.join"))));
            assert_eq!(script_words(&guest)["retry"], t!("call.guest.retry"));
            assert_eq!(script_words(&guest)["unmute_camera"], t!("call.guest.unmute_camera"));
            assert_eq!(script_words(&receiver)["waiting"], t!("call.receiver.waiting"));
        }
        set_language(Lang::Fr);
        let hostile = script_text(&[("a", "</script><script>alert(1)")]);
        assert!(!hostile.contains('<'));
        assert_eq!(serde_json::from_str::<Value>(&hostile).unwrap()["a"], "</script><script>alert(1)");
    }

    #[test]
    fn the_return_picture_keeps_the_output_shape() {
        assert_eq!(return_size(1920, 1080), (640, 360));
        assert_eq!(return_size(1080, 1920).0, 640);
        let jpg = jpeg(&Frame::solid(1920, 1080, [200, 30, 30]), 640, 360).unwrap();
        let back = image::load_from_memory(&jpg).unwrap().to_rgb8();
        assert_eq!((back.width(), back.height()), (640, 360));
        assert!(back.get_pixel(320, 180)[0] > 150);
    }
}
