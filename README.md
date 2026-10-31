<p align="center">
  <img src=".github/img/logo.svg" width="150" alt="écrou logo">
</p>
<h1 align="center">ecrou</h1>
<p align="center">a small microVM runtime based on <a href="https://github.com/libkrun/libkrun">libkrun</a></p>

## Try ecrou in the terminal

### Installation

Linux (strictly confined snap, KVM required):
```
sudo snap install ecrou
sudo snap connect ecrou:kvm
```

macOS (Apple Silicon, Homebrew):
```
brew trust --tap libkrun/krun
brew install vhaudiquet/ecrou/ecrou
```

### Usage

```
$ ecrou run -it ubuntu
root@ecrou-vm-m2glhl:/# 
```

## Embed ecrou into your project

ecrou is both a library and a tiny CLI frontend around it. The library is embeddable into any project, allowing to use microVMs easily.

### Rust

ecrou is a Rust library, which can just be called:
```rust
use ecrou::vm::RunOptions;
use ecrou::NoProgress;

fn main() -> ecrou::Result<std::process::ExitCode> {
    let code = ecrou::vm::run(
        &RunOptions {
            image: "ubuntu:resolute".into(),
            command: vec!["uname".into(), "-a".into()],
            ..RunOptions::default()
        },
        &NoProgress,
    )?;
    Ok(std::process::ExitCode::from(code.clamp(0, 255) as u8))
}
```

