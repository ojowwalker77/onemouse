//! Remembers the arrangement, and the secondary's displays so it can be
//! arranged while disconnected. Plain text, one item per line:
//!
//! ```text
//! origin 1470 46
//! display 1 0 0 1920 1080 1.25 primary
//! ```

use std::fs;
use std::io;
use std::path::Path;

use onemouse_protocol::Display;

use crate::layout::Point;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub origin: Option<Point>,
    pub displays: Vec<Display>,
}

impl Config {
    /// A missing or unreadable file is an empty config; bad lines are skipped.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Write then rename, so a crash never leaves half a file.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, self.to_string())?;
        fs::rename(tmp, path)
    }

    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for line in text.lines() {
            let words: Vec<_> = line.split_whitespace().collect();
            match words.as_slice() {
                ["origin", x, y] => {
                    if let (Ok(x), Ok(y)) = (x.parse(), y.parse()) {
                        config.origin = Some(Point::new(x, y));
                    }
                }
                ["display", id, x, y, w, h, scale, rest @ ..] => {
                    let display = (|| {
                        Some(Display {
                            id: id.parse().ok()?,
                            x: x.parse().ok()?,
                            y: y.parse().ok()?,
                            width: w.parse().ok()?,
                            height: h.parse().ok()?,
                            scale: scale.parse().ok()?,
                            primary: rest.first() == Some(&"primary"),
                        })
                    })();
                    config.displays.extend(display);
                }
                _ => {}
            }
        }
        config
    }
}

impl std::fmt::Display for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(o) = self.origin {
            writeln!(f, "origin {} {}", o.x, o.y)?;
        }
        for d in &self.displays {
            writeln!(
                f,
                "display {} {} {} {} {} {}{}",
                d.id,
                d.x,
                d.y,
                d.width,
                d.height,
                d.scale,
                if d.primary { " primary" } else { "" }
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            origin: Some(Point::new(1470.0, -46.5)),
            displays: vec![
                Display {
                    id: 7,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    scale: 1.25,
                    primary: true,
                },
                Display {
                    id: 9,
                    x: -2560,
                    y: -200,
                    width: 2560,
                    height: 1440,
                    scale: 2.0,
                    primary: false,
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("onemouse-config-{}", std::process::id()));
        let path = dir.join("nested/arrangement");
        sample().save(&path).unwrap();
        assert_eq!(Config::load(&path), sample());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tolerates_missing_files_and_junk() {
        assert_eq!(
            Config::load(Path::new("/nonexistent/onemouse")),
            Config::default()
        );
        let text = "origin 1 2\norigin x y\ndisplay 1 0 0 10 10\ndisplay 2 0 0 10 10 1.5\nhello\n";
        assert_eq!(
            Config::parse(text),
            Config {
                origin: Some(Point::new(1.0, 2.0)),
                displays: vec![Display {
                    id: 2,
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                    scale: 1.5,
                    primary: false,
                }],
            }
        );
    }
}
