//! Part of `--selftest`: a second windowless browser, with a fake camera and
//! microphone, joins a video call input through its guest link over https,
//! as a guest would. The input must show moving pictures and carry sound,
//! and the guest must receive the program back.

use crate::browser::{self, ProcessTree};
use crate::engine::Engine;
use crate::functions::Args;
use crate::model::*;
use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tungstenite::Message;

/// The guest's browser, ended with everything it started.
struct Guest {
    child: std::process::Child,
    profile: std::path::PathBuf,
    _tree: Option<ProcessTree>,
}

impl Drop for Guest {
    fn drop(&mut self) {
        crate::platform::end_process_tree(&mut self.child);
        // The tree lets go before the browser is reaped, so the number it
        // holds can never belong to another program by then.
        drop(self._tree.take());
        let _ = self.child.wait();
        sleep(Duration::from_millis(500));
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn start_guest(link: &str) -> Result<(Guest, u16), String> {
    let exe = browser::find_browser().ok_or("Edge ou Chrome introuvable")?;
    let port = browser::free_port().ok_or("aucun port libre")?;
    let profile =
        browser::private_profile("LiveCraft-web-guest", port).ok_or("dossier du navigateur impossible")?;
    let mut cmd = Command::new(exe);
    cmd.args(["--headless=new", "--no-first-run", "--no-default-browser-check", "--mute-audio"])
        .args(["--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"])
        // The certificate is made for this run: a guest accepts it once.
        .arg("--ignore-certificate-errors")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::prepare_child(&mut cmd);
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let tree = ProcessTree::hold(&child);
    Ok((Guest { child, profile, _tree: tree }, port))
}

/// Control of the guest's page, found by its address.
fn guest_page(port: u16) -> Result<browser::Socket, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let list =
            browser::http(port, "GET", "/json/list").and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let found = list.as_ref().and_then(|l| l.as_array()).and_then(|l| {
            l.iter().find(|t| t["type"] == "page" && t["url"].as_str().is_some_and(|u| u.contains("/call/")))
        });
        if let Some(address) = found.and_then(|t| t["webSocketDebuggerUrl"].as_str())
            && let Ok((mut ws, _)) = tungstenite::connect(address)
        {
            if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            }
            return Ok(ws);
        }
        sleep(Duration::from_millis(200));
    }
    Err("la page de l'invité ne s'ouvre pas".into())
}

/// Runs a script in the guest's page and returns its value.
fn evaluate(ws: &mut browser::Socket, id: &mut u64, expression: &str) -> Result<Value, String> {
    *id += 1;
    let params =
        json!({ "expression": expression, "awaitPromise": true, "returnByValue": true, "userGesture": true });
    if !browser::send(ws, *id, "Runtime.evaluate", params) {
        return Err("commande refusée par le navigateur de l'invité".into());
    }
    // Longer than the desk waits for its own page to answer a guest (20 s),
    // so a slow answer is told apart from none.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let Ok(Message::Text(text)) = ws.read() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
        if v["id"] == *id {
            // A script that throws would otherwise look like one that
            // returned nothing, and the check would go on blind.
            if let Some(e) = v["result"]["exceptionDetails"].as_object().or(v["error"].as_object()) {
                let text = e.get("exception").and_then(|x| x["description"].as_str());
                let text = text.or(e.get("text").and_then(Value::as_str));
                let text = text.or(e.get("message").and_then(Value::as_str)).unwrap_or("?");
                return Err(format!("{expression} : {text}"));
            }
            return Ok(v["result"]["result"]["value"].clone());
        }
    }
    Err(format!("pas de réponse à {expression}"))
}

/// The page is listed as soon as its address is asked for, well before its
/// script has run: until then there is nothing to call.
fn wait_for_script(ws: &mut browser::Socket, id: &mut u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let ready = "document.readyState=='complete'&&typeof join=='function'";
    while Instant::now() < deadline {
        // While the page loads, its context may be replaced under the call.
        if evaluate(ws, id, ready) == Ok(Value::Bool(true)) {
            return Ok(());
        }
        sleep(Duration::from_millis(200));
    }
    let seen = evaluate(ws, id, "location.href+' / '+document.title")?;
    Err(format!("la page de l'invité ne se charge pas : {seen}"))
}

pub fn check(engine: &Arc<Engine>) -> Result<String, String> {
    let id = engine.new_id();
    let kind = crate::call::new_call();
    let Kind::VideoCall { token, .. } = &kind else { unreachable!() };
    let link = crate::call::guest_link(token, "", false).ok_or("lien absent")?;
    engine.add_input(Input::new(id, "Invité", kind.clone()));
    let rt = engine.rt(id).ok_or("entrée absente")?;
    let link_state = rt.call.clone().ok_or("entrée sans appel")?;
    // In preview, as an operator checks a guest: a source nobody watches
    // draws only a couple of pictures a second.
    engine.call("PreviewInput", &Args::new().input("Invité"))?;
    // The guest is sent the link once the input shows it waits for them, as
    // an operator would; until then its browser may still be starting.
    let shown = Instant::now();
    while rt.seq() == 0 {
        if shown.elapsed() > Duration::from_secs(30) {
            return Err(format!("l'entrée d'appel ne montre rien : {:?}", rt.status()));
        }
        sleep(Duration::from_millis(100));
    }

    let (_guest, port) = start_guest(&link)?;
    let mut ws = guest_page(port)?;
    let mut n = 0;
    wait_for_script(&mut ws, &mut n)?;
    evaluate(&mut ws, &mut n, "join()")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !link_state.is_connected() {
        if Instant::now() > deadline {
            let state = evaluate(
                &mut ws,
                &mut n,
                "document.body.dataset.state+' / '+document.getElementById('s').textContent",
            )?;
            return Err(format!("l'invité ne se connecte pas : {state} ; entrée : {:?}", rt.status()));
        }
        sleep(Duration::from_millis(100));
    }

    // Pictures are only published when they change, so a count that keeps
    // rising means the guest's camera is moving on the input.
    sleep(Duration::from_secs(1));
    let (seq, first) = (rt.seq(), rt.latest().ok_or("aucune image de l'invité")?);
    let mut loudest = 0f32;
    let listen = Instant::now();
    while listen.elapsed() < Duration::from_secs(4) {
        loudest = loudest.max(rt.peak());
        sleep(Duration::from_millis(10));
    }
    let pictures = rt.seq() - seq;
    let last = rt.latest().ok_or("image perdue")?;
    if pictures < 30 || last.data == first.data {
        return Err(format!("image de l'invité figée : {pictures} images en 4 s"));
    }
    if loudest < 0.01 {
        return Err(format!("aucun son de l'invité (crête {loudest})"));
    }
    let back = evaluate(
        &mut ws,
        &mut n,
        "(()=>{const v=document.getElementById('back');\
         return [v.videoWidth,v.currentTime,v.srcObject?v.srcObject.getAudioTracks().length:0]})()",
    )?;
    let (width, time, sound) =
        (back[0].as_f64().unwrap_or(0.0), back[1].as_f64().unwrap_or(0.0), back[2].as_u64());
    if width < 1.0 || time <= 0.0 || sound != Some(1) {
        return Err(format!("l'invité ne reçoit pas le programme : {back}"));
    }
    // Closing the input must end the call, not leave it running unseen.
    engine.close_input(id);
    let gone = Instant::now();
    while link_state.is_connected() {
        if gone.elapsed() > Duration::from_secs(5) {
            return Err("l'appel continue après la fermeture de l'entrée".into());
        }
        sleep(Duration::from_millis(100));
    }
    Ok(format!("appel vidéo : {pictures} images en 4 s, crête son {loudest:.2}, retour {width} px"))
}
