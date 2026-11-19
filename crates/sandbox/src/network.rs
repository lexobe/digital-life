use crate::config::{GATEWAY, GATEWAY_PORT, HOST_LINK, ROUTER};
use crate::{SandboxConfig, SandboxSpec, source_ip};
use anyhow::Result;
use std::collections::BTreeMap;

pub(crate) const PRIVATE: &str = "0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.0.0.0/24, 192.0.2.0/24, 192.168.0.0/16, 198.18.0.0/15, 198.51.100.0/24, 203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4";

/// Atomically replaces only our table, inside our router namespace. There is no
/// L2 bridge between agents, so ARP and link-local traffic cannot bypass policy.
pub(crate) fn router_rules(config: &SandboxConfig, specs: &[SandboxSpec]) -> Result<String> {
    let mut peers: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for s in specs {
        s.validate(config)?;
        if s.network.peers {
            peers
                .entry(&s.experiment_id)
                .or_default()
                .push(source_ip(s.slot)?.to_string());
        }
    }
    let extra = if config.denied_cidrs.is_empty() {
        String::new()
    } else {
        format!(", {}", config.denied_cidrs.join(", "))
    };
    let mut out = format!(
        "delete table inet survive\nadd table inet survive\nadd chain inet survive input {{ type filter hook input priority -200; policy drop; }}\nadd rule inet survive input iifname lo accept\nadd chain inet survive forward {{ type filter hook forward priority -200; policy drop; }}\nadd rule inet survive forward meta nfproto != ipv4 drop\nadd rule inet survive forward ct state invalid drop\nadd set inet survive denied {{ type ipv4_addr; flags interval; auto-merge; elements = {{ {PRIVATE}{extra} }}; }}\n"
    );
    for (index, ips) in peers.values().enumerate() {
        out.push_str(&format!(
            "add set inet survive peers{index} {{ type ipv4_addr; elements = {{ {} }}; }}\n",
            ips.join(", ")
        ));
    }
    for s in specs {
        let ip = source_ip(s.slot)?;
        let link = s.link();
        let chain = format!("agent{}", s.slot);
        out.push_str(&format!("add chain inet survive {chain}\nadd rule inet survive forward iifname \"{link}\" ip saddr != {ip} drop\nadd rule inet survive forward iifname \"{link}\" ip saddr {ip} jump {chain}\n"));
        out.push_str(&format!("add rule inet survive {chain} oifname uplink ip daddr {GATEWAY} tcp dport {GATEWAY_PORT} accept\n"));
        if s.network.services {
            out.push_str(&format!("add rule inet survive {chain} oifname uplink ip daddr {GATEWAY} meta l4proto tcp ct direction reply ct original ip saddr {GATEWAY} ct state established accept\nadd rule inet survive forward iifname uplink oifname \"{link}\" ip saddr {GATEWAY} ip daddr {ip} meta l4proto tcp accept\n"));
        }
        if s.network.peers {
            let index = peers
                .keys()
                .position(|key| *key == s.experiment_id)
                .unwrap();
            out.push_str(&format!(
                "add rule inet survive {chain} oifname \"sa*\" ip daddr @peers{index} accept\n"
            ));
        }
        out.push_str(&format!(
            "add rule inet survive {chain} ip daddr @denied drop\n"
        ));
        if s.network.internet {
            out.push_str(&format!(
                "add rule inet survive {chain} oifname uplink accept\n"
            ));
        }
        out.push_str(&format!("add rule inet survive {chain} drop\nadd rule inet survive forward iifname uplink oifname \"{link}\" ip daddr {ip} ip saddr {GATEWAY} tcp sport {GATEWAY_PORT} ct state established accept\n"));
        if s.network.internet {
            out.push_str(&format!("add rule inet survive forward iifname uplink oifname \"{link}\" ip daddr {ip} ip saddr != @denied ct state established,related accept\n"));
        }
    }
    Ok(out)
}

pub(crate) fn host_rules(specs: &[SandboxSpec]) -> Result<String> {
    let services = specs
        .iter()
        .filter(|s| s.network.services)
        .map(|s| source_ip(s.slot).map(|ip| ip.to_string()))
        .collect::<Result<Vec<_>>>()?;
    let elements = if services.is_empty() {
        String::new()
    } else {
        format!("elements = {{ {} }};", services.join(", "))
    };
    // OUTPUT runs after conntrack (-200), otherwise gateway replies to agents
    // with services disabled can be classified before their connection exists.
    // No DNAT, no wildcard accept, no change to ports 80/443 or Docker tables.
    Ok(format!(
        r#"delete table inet survive_host
add table inet survive_host
add set inet survive_host services {{ type ipv4_addr; {elements} }}
add chain inet survive_host input {{ type filter hook input priority -200; policy accept; }}
add rule inet survive_host input ip daddr {GATEWAY} tcp dport {GATEWAY_PORT} iifname != "{HOST_LINK}" drop
add rule inet survive_host input iifname "{HOST_LINK}" ip saddr 10.203.0.0/16 ip daddr {GATEWAY} tcp dport {GATEWAY_PORT} accept
add rule inet survive_host input iifname "{HOST_LINK}" ip saddr @services ip daddr {GATEWAY} meta l4proto tcp ct direction reply ct original ip saddr {GATEWAY} ct state established accept
add rule inet survive_host input iifname "{HOST_LINK}" drop
add chain inet survive_host forward {{ type filter hook forward priority -200; policy accept; }}
add rule inet survive_host forward iifname "{HOST_LINK}" meta nfproto != ipv4 drop
add rule inet survive_host forward iifname "{HOST_LINK}" ip saddr != 10.203.0.0/16 drop
add rule inet survive_host forward iifname "{HOST_LINK}" ip daddr {{ {PRIVATE} }} drop
add rule inet survive_host forward oifname "{HOST_LINK}" ip daddr 10.203.0.0/16 ct state established,related accept
add rule inet survive_host forward oifname "{HOST_LINK}" drop
add chain inet survive_host output {{ type filter hook output priority -150; policy accept; }}
add rule inet survive_host output oifname "{HOST_LINK}" ip saddr {GATEWAY} tcp sport {GATEWAY_PORT} ct state established accept
add rule inet survive_host output oifname "{HOST_LINK}" ip saddr {GATEWAY} ip daddr @services meta l4proto tcp accept
add rule inet survive_host output oifname "{HOST_LINK}" drop
add chain inet survive_host nat {{ type nat hook postrouting priority 110; policy accept; }}
add rule inet survive_host nat iifname "{HOST_LINK}" ip saddr 10.203.0.0/16 masquerade
"#
    ))
}

pub(crate) fn apply(config: &SandboxConfig, specs: &[SandboxSpec]) -> Result<()> {
    crate::system::ns_command(
        ROUTER,
        "nft",
        &["-f", "-"],
        Some(&router_rules(config, specs)?),
        config.command_timeout_secs,
    )?;
    crate::system::run(
        &crate::system::tool("nft")?,
        &["-f", "-"],
        Some(&host_rules(specs)?),
        config.command_timeout_secs,
    )?;
    Ok(())
}
