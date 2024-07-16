# taper

Create and connect [tap](https://docs.kernel.org/networking/tuntap.html)
devices via `TCP`. This is more or less a very very basic VPN.

It's not secure, it's not optimized for speed, it's not idiot proof. It's just a
dev tool.

The intention for `taper` is to use it for accessing locally connected
development board or targets from remote dev machines.

## Install

```sh
cargo install --path .
sudo setcap cap_net_raw,cap_net_admin+eip ~/.cargo/bin/taper
```

## Usage

```sh
Taper - crate and connect tap devices via TCP (or stdio)

Usage: taper [OPTIONS] <COMMAND>

Commands:
  server  Server mode
  client  Client mode
  stdio   Stdio mode
  help    Print this message or the help of the given subcommand(s)

Options:
  -b, --bridge <BRIDGE>  Attach tap device to bridge. The bridge is created if it does not exist. `ip link add name <bridge> type bridge && ip link set <bridge> up`
  -a, --attach <ATTACH>  Attach devices to bridge: `ip link set <dev> master bridge`
  -h, --help             Print help
  -V, --version          Print version
```
