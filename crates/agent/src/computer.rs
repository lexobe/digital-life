//! Desktop actions target the X server inside this sandbox. Commands use argv directly.
use crate::journal::atomic_write;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, io::Read, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

use survive_core::{MAX_FRAME_BYTES, is_desktop_jpeg};
pub(crate) const FRAME_FILE: &str = "screen.jpg";

pub(crate) fn definition() -> Value {
    let coord = json!({"type":"integer","minimum":0});
    json!({"type":"function","function":{
        "name":"computer",
        "description":"Operate your sandbox desktop and browser. screenshot supplies pixels to the next inference. Coordinates are screenshot pixels from top left. click/double_click/move/scroll/drag require x,y; drag also requires to_x,to_y. scroll uses direction and optional steps. type pastes Unicode text via Ctrl+V (set terminal=true for Ctrl+Shift+V). key accepts one chord such as Ctrl+l, Return, Alt+Tab, Super+e. wait uses ms. Take a screenshot after actions to see the result.",
        "parameters":{"type":"object","additionalProperties":false,"required":["action"],"properties":{
            "action":{"type":"string","enum":["screenshot","click","double_click","move","drag","scroll","type","key","wait"]},
            "x":coord,"y":coord,"to_x":coord,"to_y":coord,
            "button":{"type":"string","enum":["left","middle","right"]},
            "direction":{"type":"string","enum":["up","down","left","right"]},
            "steps":{"type":"integer","minimum":1,"maximum":20},
            "text":{"type":"string","maxLength":4096},"terminal":{"type":"boolean"},"key":{"type":"string"},
            "ms":{"type":"integer","minimum":0,"maximum":10000}
        }}
    }})
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Button {
    Left,
    Middle,
    Right,
}
impl Button {
    fn number(&self) -> &'static str {
        match self {
            Self::Left => "1",
            Self::Middle => "2",
            Self::Right => "3",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Screenshot {},
    Click {
        x: u32,
        y: u32,
        button: Option<Button>,
    },
    DoubleClick {
        x: u32,
        y: u32,
        button: Option<Button>,
    },
    Move {
        x: u32,
        y: u32,
    },
    Drag {
        x: u32,
        y: u32,
        to_x: u32,
        to_y: u32,
        button: Option<Button>,
    },
    Scroll {
        x: u32,
        y: u32,
        direction: Direction,
        steps: Option<u8>,
    },
    Type {
        text: String,
        #[serde(default)]
        terminal: bool,
    },
    Key {
        key: String,
    },
    Wait {
        ms: u64,
    },
}

#[derive(Clone, Copy)]
struct Screen {
    width: u32,
    height: u32,
}

impl Screen {
    fn motion(self, x: u32, y: u32) -> Result<Vec<String>> {
        ensure!(
            x < self.width && y < self.height,
            "point ({x}, {y}) outside {}x{} desktop",
            self.width,
            self.height
        );
        Ok(vec!["mousemove".into(), x.to_string(), y.to_string()])
    }
}

fn pointer_args(action: &Action, screen: Screen) -> Result<Vec<String>> {
    let mut args = match action {
        Action::Click { x, y, .. }
        | Action::DoubleClick { x, y, .. }
        | Action::Move { x, y }
        | Action::Drag { x, y, .. }
        | Action::Scroll { x, y, .. } => screen.motion(*x, *y)?,
        _ => bail!("expected pointer action"),
    };
    match action {
        Action::Click { button, .. } | Action::DoubleClick { button, .. } => {
            let count = if matches!(action, Action::DoubleClick { .. }) {
                "2"
            } else {
                "1"
            };
            let button = button.as_ref().unwrap_or(&Button::Left).number();
            args.extend(["click", "--repeat", count, "--delay", "80", button].map(String::from));
        }
        Action::Drag {
            x,
            y,
            to_x,
            to_y,
            button,
        } => {
            let destination = screen.motion(*to_x, *to_y)?;
            let button = button.as_ref().unwrap_or(&Button::Left).number();
            args.extend(["mousedown", button, "sleep", "0.08"].map(String::from));
            args.extend(screen.motion(x.midpoint(*to_x), y.midpoint(*to_y))?);
            args.extend(["sleep", "0.08"].map(String::from));
            args.extend(destination);
            args.extend(["sleep", "0.08", "mouseup", button].map(String::from));
        }
        Action::Scroll {
            direction, steps, ..
        } => {
            let button = match direction {
                Direction::Up => "4",
                Direction::Down => "5",
                Direction::Left => "6",
                Direction::Right => "7",
            };
            let steps = steps.unwrap_or(3);
            ensure!((1..=20).contains(&steps), "scroll steps must be 1..20");
            args.extend(
                [
                    "click",
                    "--repeat",
                    &steps.to_string(),
                    "--delay",
                    "40",
                    button,
                ]
                .map(String::from),
            );
        }
        _ => {}
    }
    Ok(args)
}

fn key_chord(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && value.len() <= 80,
        "expected one key chord"
    );
    let parts: Vec<_> = value.split('+').collect();
    ensure!(parts.len() <= 5, "at most four modifiers");
    let mut keys = Vec::new();
    for modifier in &parts[..parts.len() - 1] {
        let key = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "ctrl",
            "alt" => "alt",
            "shift" => "shift",
            "super" | "win" | "meta" => "Super_L",
            _ => bail!("unknown modifier {modifier}"),
        };
        ensure!(!keys.contains(&key), "duplicate modifier {modifier}");
        keys.push(key);
    }
    let last = parts.last().unwrap().to_ascii_lowercase();
    let function_key = last
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .filter(|n| (1..=24).contains(n));
    if let Some(number) = function_key {
        let name = format!("F{number}");
        keys.push(&name);
        return Ok(keys.join("+"));
    }
    let key = match last.as_str() {
        "enter" | "return" => "Return",
        "escape" | "esc" => "Escape",
        "space" => "space",
        "tab" => "Tab",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Prior",
        "pagedown" | "page_down" => "Next",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "minus" | "-" => "minus",
        "plus" => "plus",
        "equal" | "=" => "equal",
        "slash" | "/" => "slash",
        "period" | "." => "period",
        "comma" | "," => "comma",
        "bracketleft" | "[" => "bracketleft",
        "bracketright" | "]" => "bracketright",
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => key,
        _ => bail!("unsupported key {last}"),
    };
    keys.push(key);
    Ok(keys.join("+"))
}

fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove("SURVIVE_TOKEN")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

async fn xdotool(args: &[String]) -> Result<Vec<u8>> {
    let output = timeout(
        Duration::from_secs(8),
        command("xdotool").args(args).output(),
    )
    .await
    .context("xdotool timed out")?
    .context("starting xdotool")?;
    ensure!(
        output.status.success(),
        "xdotool failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

async fn geometry() -> Result<Screen> {
    let bytes = xdotool(&["getdisplaygeometry".into()]).await?;
    let mut parts = std::str::from_utf8(&bytes)?.split_whitespace();
    let width: u32 = parts.next().context("missing display width")?.parse()?;
    let height: u32 = parts.next().context("missing display height")?.parse()?;
    ensure!(
        width > 0 && height > 0 && parts.next().is_none(),
        "invalid display geometry"
    );
    Ok(Screen { width, height })
}

pub(crate) struct Frame {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn local_display() -> Result<()> {
    let display = std::env::var("DISPLAY").context("sandbox desktop DISPLAY is unavailable")?;
    ensure!(
        display == ":0" || display == ":0.0",
        "desktop actions require the local sandbox display :0"
    );
    Ok(())
}

pub(crate) async fn capture(directory: &Path) -> Result<Frame> {
    local_display()?;
    let screen = geometry().await?;
    ensure!(
        screen.width <= 1280 && screen.height <= 720,
        "screenshot exceeds supported 1280x720 desktop"
    );
    // scrot interprets format specifiers in filenames; use constant basenames and cwd.
    let candidate = directory.join("capture.jpg");
    for quality in ["80", "55", "30", "10", "1"] {
        let output = timeout(
            Duration::from_secs(5),
            command("scrot")
                .current_dir(directory)
                .args(["-z", "-p", "-o", "-q", quality, "capture.jpg"])
                .output(),
        )
        .await
        .context("screenshot timed out")?
        .context("starting scrot")?;
        ensure!(
            output.status.success(),
            "scrot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if fs::metadata(&candidate)?.len() <= MAX_FRAME_BYTES as u64 {
            let mut bytes = Vec::new();
            fs::File::open(&candidate)?
                .take(MAX_FRAME_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                is_desktop_jpeg(&bytes),
                "capture is not a bounded desktop JPEG"
            );
            fs::remove_file(candidate)?;
            return Ok(Frame {
                bytes,
                width: screen.width,
                height: screen.height,
            });
        }
    }
    fs::remove_file(candidate)?;
    bail!("screenshot exceeds {MAX_FRAME_BYTES} bytes at lowest JPEG quality")
}

async fn paste(text: &str, terminal: bool) -> Result<()> {
    ensure!(
        !text.is_empty() && text.chars().count() <= 4096 && !text.contains('\0'),
        "type expects 1..4096 Unicode characters without NUL"
    );
    // xclip forks a selection owner after reading stdin. Its inherited descriptors
    // must not keep pipes open. The next clipboard owner supersedes it.
    let mut child = command("xclip")
        .args(["-in", "-selection", "clipboard", "-target", "UTF8_STRING"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting xclip")?;
    let mut stdin = child.stdin.take().context("opening clipboard input")?;
    timeout(Duration::from_secs(5), async {
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin);
        ensure!(child.wait().await?.success(), "xclip failed");
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("clipboard timed out")??;
    let chord = if terminal { "ctrl+shift+v" } else { "ctrl+v" };
    xdotool(&["key".into(), "--clearmodifiers".into(), chord.into()]).await?;
    Ok(())
}

pub(crate) async fn execute(arguments: &str, directory: &Path) -> Result<Value> {
    let action: Action = serde_json::from_str(arguments)?;
    local_display()?;
    match &action {
        Action::Screenshot {} => {
            let frame = capture(directory).await?;
            let path = directory.join(FRAME_FILE);
            atomic_write(&path, &frame.bytes)?;
            return Ok(
                json!({"ok":true,"action":"screenshot","path":path,"bytes":frame.bytes.len(),"width":frame.width,"height":frame.height}),
            );
        }
        Action::Type { text, terminal } => paste(text, *terminal).await?,
        Action::Key { key } => {
            xdotool(&["key".into(), "--clearmodifiers".into(), key_chord(key)?]).await?;
        }
        Action::Wait { ms } => {
            ensure!(*ms <= 10_000, "wait must be at most 10000 ms");
            tokio::time::sleep(Duration::from_millis(*ms)).await;
        }
        _ => {
            xdotool(&pointer_args(&action, geometry().await?)?).await?;
        }
    }
    Ok(json!({"ok":true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(action: Value) -> Result<String> {
        let action = serde_json::from_value(action)?;
        Ok(pointer_args(
            &action,
            Screen {
                width: 1280,
                height: 720,
            },
        )?
        .join(" "))
    }

    #[test]
    fn pointer_coordinates_buttons_drag_release_and_scroll_match_x11() {
        let cases = [
            (
                json!({"action":"click","x":0,"y":0}),
                "mousemove 0 0 click --repeat 1 --delay 80 1",
            ),
            (
                json!({"action":"double_click","x":1279,"y":719,"button":"right"}),
                "mousemove 1279 719 click --repeat 2 --delay 80 3",
            ),
            (json!({"action":"move","x":20,"y":60}), "mousemove 20 60"),
            (
                json!({"action":"drag","x":10,"y":20,"to_x":110,"to_y":220,"button":"middle"}),
                "mousemove 10 20 mousedown 2 sleep 0.08 mousemove 60 120 sleep 0.08 mousemove 110 220 sleep 0.08 mouseup 2",
            ),
        ];
        for (action, expected) in cases {
            assert_eq!(pointer(action).unwrap(), expected);
        }
        for (direction, button) in [("up", 4), ("down", 5), ("left", 6), ("right", 7)] {
            assert_eq!(
                pointer(json!({"action":"scroll","x":0,"y":300,"direction":direction,"steps":2}))
                    .unwrap(),
                format!("mousemove 0 300 click --repeat 2 --delay 40 {button}")
            );
        }
        for action in [
            json!({"action":"move","x":1280,"y":0}),
            json!({"action":"click","x":0,"y":720}),
            json!({"action":"drag","x":0,"y":0,"to_x":1280,"to_y":2}),
            json!({"action":"scroll","x":0,"y":0,"steps":21,"direction":"up"}),
            json!({"action":"move","x":2,"y":3,"display":":2"}),
        ] {
            assert!(pointer(action).is_err());
        }
    }

    #[test]
    fn one_key_chord_maps_to_one_x11_argument() {
        for (arg, expected) in [
            ("Ctrl+Shift+Tab", "ctrl+shift+Tab"),
            ("Enter", "Return"),
            ("page_down", "Next"),
            ("Ctrl+l", "ctrl+l"),
            ("ALT+F4", "alt+F4"),
            ("Super+E", "Super_L+e"),
        ] {
            assert_eq!(key_chord(arg).unwrap(), expected);
        }
        for key in [
            "Ctrl+Ctrl+v",
            "enter mouseup 1",
            "F25",
            "--window",
            "Ctrl+",
            "+",
            "s;r",
        ] {
            assert!(key_chord(key).is_err(), "{key}");
        }
    }
}
