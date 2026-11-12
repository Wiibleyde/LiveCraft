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
use crate::model::{Kind, Project, ReturnSound};
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
/// How long a guest's request waits for the desk's answer.
const ANSWER_WAIT: Duration = Duration::from_secs(20);
/// What the page may need to answer: it gathers its addresses for up to 3 s.
/// An offer with less time left is not started, as its guest would be gone
/// by the time the answer is ready, and the page would have dropped for it
/// the guest it had.
const ANSWER_TIME: Duration = Duration::from_secs(5);
/// Offers waiting for the page. A newer one pushes the oldest out, whose
/// guest is told at once.
const MAX_WAITING: usize = 4;
type Answer = mpsc::Sender<Result<String, String>>;

/// A guest's offer and the request waiting for its answer.
struct Waiting {
    id: u64,
    offer: String,
    answer: Answer,
    until: Instant,
}

/// What passes between a running call and the rest of the desk.
pub struct CallLink {
    offers: Mutex<Vec<Waiting>>,
    next_offer: AtomicU64,
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
            next_offer: AtomicU64::new(0),
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
        self.answer_within(offer, ANSWER_WAIT)
    }

    fn answer_within(&self, offer: String, wait: Duration) -> Result<String, String> {
        let (id, rx) = self.queue(offer, wait);
        let got = rx.recv_timeout(wait);
        // Answered, pushed out or given up: in no case may the page take it
        // up later, for a guest who is no longer waiting.
        self.offers.lock().unwrap().retain(|w| w.id != id);
        got.map_err(|_| t!("call.no_answer").to_string())?
    }

    fn queue(&self, offer: String, wait: Duration) -> (u64, mpsc::Receiver<Result<String, String>>) {
        let (answer, rx) = mpsc::channel();
        let id = self.next_offer.fetch_add(1, Ordering::Relaxed);
        let mut offers = self.offers.lock().unwrap();
        while offers.len() >= MAX_WAITING {
            // Its sender goes with it, which ends its guest's wait.
            offers.remove(0);
        }
        offers.push(Waiting { id, offer, answer, until: Instant::now() + wait });
        (id, rx)
    }

    /// The offers the page can still answer in time. The others are let go,
    /// and their guests told so at once.
    fn take_answerable(&self) -> Vec<(String, Answer)> {
        let now = Instant::now();
        let waiting = std::mem::take(&mut *self.offers.lock().unwrap());
        waiting
            .into_iter()
            .filter(|w| w.until.saturating_duration_since(now) >= ANSWER_TIME)
            .map(|w| (w.offer, w.answer))
            .collect()
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

/// What a copy of an input starts as. A copied call gets a link of its own:
/// one link for two inputs would let its holder into either.
pub fn copy_kind(kind: &Kind) -> Kind {
    let mut kind = kind.clone();
    if let Kind::VideoCall { token, .. } = &mut kind {
        *token = new_token();
    }
    kind
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
    let mut hook = CallPage::new(rt.clone(), link.clone(), (rw, rh));
    browser::run_page(rt, &url, false, (w, h), fps, &mut hook);
    link.set_connected(false);
}

/// Talks to the page receiving the call.
struct CallPage {
    rt: Arc<InputRt>,
    link: Arc<CallLink>,
    answers: HashMap<u64, Answer>,
    /// The page has loaded and can answer. An offer taken before would run
    /// on the blank page the tab opens with and be refused.
    ready: bool,
    size: (usize, usize),
    sent_seq: u64,
    last_picture: Instant,
    last_sound: Instant,
    pcm: Vec<f32>,
}

impl CallPage {
    fn new(rt: Arc<InputRt>, link: Arc<CallLink>, size: (usize, usize)) -> Self {
        Self {
            rt,
            link,
            answers: HashMap::new(),
            ready: false,
            size,
            sent_seq: 0,
            last_picture: Instant::now(),
            last_sound: Instant::now(),
            pcm: Vec::new(),
        }
    }

    /// Offers waiting for the page. Until it has loaded they stay queued,
    /// and the guest's request waits for its answer.
    fn take_offers(&mut self) -> Vec<(String, Answer)> {
        if !self.ready {
            return Vec::new();
        }
        self.link.take_answerable()
    }

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
        self.ready = false;
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
        for (offer, tx) in self.take_offers() {
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
                Some("lcState") if payload == "ready" => self.ready = true,
                Some("lcState") => self.state(payload),
                _ => {}
            }
        } else if let Some(tx) = v["id"].as_u64().and_then(|n| self.answers.remove(&n)) {
            let sdp = v["result"]["result"]["value"].as_str().map(String::from);
            let _ = tx.send(sdp.ok_or_else(|| t!("call.refused").to_string()));
        }
    }
}

/// Where a guest link leads.
pub struct GuestRoute {
    pub link: Arc<CallLink>,
    /// The STUN and TURN servers, as the pages expect them.
    pub ice: String,
    /// The link's address is an https relay, which may pass requests on in
    /// plain http.
    pub relayed: bool,
}

/// The input a guest link opens, its servers, and whether it goes through a
/// relay. A link found on more than one input opens none: it would be the
/// key to someone else's call.
fn call_input(project: &Project, token: &str) -> Option<(u64, String, bool)> {
    let mut found = project.inputs.iter().filter_map(|i| match &i.kind {
        Kind::VideoCall { token: real, address, stun, turn, turn_user, turn_password, .. }
            if same_token(token, real) =>
        {
            let relayed = address.trim().get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"));
            Some((i.id, ice_servers(stun, turn, turn_user, turn_password), relayed))
        }
        _ => None,
    });
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

impl Engine {
    /// The running call a guest link opens.
    pub(crate) fn guest_route(&self, token: &str) -> Option<GuestRoute> {
        let (id, ice, relayed) = call_input(&self.state.lock().unwrap().project, token)?;
        Some(GuestRoute { link: self.rt(id)?.call.clone()?, ice, relayed })
    }

    /// The page a link opens, as the remote control serves it to a guest.
    #[cfg(test)]
    pub fn guest_page(&self, token: &str) -> Option<String> {
        Some(guest_html(&self.guest_route(token)?.ice))
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
pub(crate) fn guest_html(ice: &str) -> String {
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
lcState('ready')
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

    /// A guest who joins while the receiving page is still loading waits
    /// for it, rather than being turned away by the blank page before it.
    #[test]
    fn an_offer_waits_until_the_receiving_page_has_loaded() {
        let link = Arc::new(CallLink::default());
        let mut page = CallPage::new(InputRt::new(true, true), link.clone(), (640, 360));
        let _waiting = link.queue("v=0".into(), ANSWER_WAIT);
        assert!(page.take_offers().is_empty(), "nothing is asked of a page still loading");
        assert_eq!(link.offers.lock().unwrap().len(), 1, "the offer stays queued");
        let called = |name: &str, payload: &str| json!({ "method": "Runtime.bindingCalled", "params": { "name": name, "payload": payload } });
        page.message(&called("lcState", "ready"));
        assert!(!link.is_connected() && page.rt.status().is_none(), "loaded is not a guest yet");
        assert_eq!(page.take_offers().len(), 1);
        assert!(link.offers.lock().unwrap().is_empty());
    }

    /// Offers nobody can answer must not pile up, nor be answered later:
    /// the page would drop the guest it has for one who has left.
    #[test]
    fn waiting_offers_stay_few_and_never_outlive_their_guest() {
        let link = CallLink::default();
        let waits: Vec<_> = (0..6).map(|n| link.queue(n.to_string(), ANSWER_WAIT).1).collect();
        assert_eq!(link.offers.lock().unwrap().len(), MAX_WAITING);
        for (n, rx) in waits.iter().enumerate() {
            let told = matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
            assert_eq!(told, n < 2, "offer {n}: the oldest are let go, and their guests told at once");
        }
        let taken: Vec<String> = link.take_answerable().into_iter().map(|(o, _)| o).collect();
        assert_eq!(taken, ["2", "3", "4", "5"], "the newest win");

        // A guest who gave up leaves nothing behind.
        assert!(link.answer_within("gone".into(), Duration::from_millis(30)).is_err());
        assert!(link.offers.lock().unwrap().is_empty());

        // One with too little time left is not started, and is told so.
        let (_, rx) = link.queue("late".into(), ANSWER_TIME / 2);
        assert!(link.take_answerable().is_empty());
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
    }

    /// A copied call is a call of its own: same servers, its own link.
    #[test]
    fn a_copied_call_has_a_link_of_its_own() {
        let mut call = new_call();
        if let Kind::VideoCall { stun, turn, .. } = &mut call {
            *stun = "stun:s:3478".into();
            *turn = "turn:t:3478".into();
        }
        let copy = copy_kind(&call);
        let (
            Kind::VideoCall { token: a, stun, turn, .. },
            Kind::VideoCall { token: b, stun: s, turn: t, .. },
        ) = (&call, &copy)
        else {
            panic!()
        };
        assert_ne!(a, b);
        assert_eq!(b.len(), 32);
        assert_eq!((stun, turn), (s, t));
        assert_eq!(copy_kind(&Kind::Bars), Kind::Bars);
    }

    /// One link, one input: a token found twice opens nothing.
    #[test]
    fn a_link_on_two_inputs_opens_neither() {
        use crate::model::Input;
        let with = |token: &str, address: &str| {
            let mut call = new_call();
            if let Kind::VideoCall { token: t, address: a, .. } = &mut call {
                (*t, *a) = (token.into(), address.into());
            }
            call
        };
        let mut p = Project::blank();
        p.inputs.push(Input::new(1, "A", with("aaaa", "")));
        p.inputs.push(Input::new(2, "B", with("bbbb", "https://relay.example")));
        assert_eq!(call_input(&p, "aaaa").map(|(id, _, relayed)| (id, relayed)), Some((1, false)));
        assert_eq!(call_input(&p, "bbbb").map(|(id, _, relayed)| (id, relayed)), Some((2, true)));
        p.inputs.push(Input::new(3, "C", with("aaaa", "")));
        assert!(call_input(&p, "aaaa").is_none());
        assert!(call_input(&p, "").is_none());
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
