//! The source description: the outside hypervisor's XML for one VM (`onevm show -x`), read into a typed
//! [`SourceVm`].
//!
//! Every value that later reaches Incus, Ceph or a guest is checked here: a MAC is six hex octets, an address parses as
//! one, a Ceph pool or image name and a bridge name hold only the characters such names hold, a port range only digits,
//! commas and colons. A value that fails is refused with the place it was found, so a proposal whose description Arca
//! cannot read is refused before the owner is asked. Text that only reaches the card (the VM's name, a group's name) is
//! kept as it is; the card cleans it when it shows it.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use base64::Engine as _;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use sha2::{Digest as _, Sha256};

/// The longest source description this build reads, in bytes: it travels inside one MCP request, which the HTTP door
/// caps at 64 KiB.
pub const MAX_SOURCE_BYTES: usize = 49_152;
/// How deep the description's elements may nest.
const MAX_DEPTH: usize = 12;
/// The most disks, NICs or security-group rules one description may hold.
const MAX_ITEMS: usize = 64;

/// One VM of the outside hypervisor, as its description says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceVm {
    /// The VM's id in the outside hypervisor.
    pub id: u64,
    pub name: String,
    /// The state the description was read in, in words (`running`, `powered off`, …), when it says one.
    pub state: Option<&'static str>,
    pub vcpu: u32,
    pub memory_mib: u64,
    /// The memory hot-plug ceiling, when the source has one. Not carried.
    pub memory_max_mib: Option<u64>,
    pub firmware: Firmware,
    /// The source's machine type, such as `pc-q35-8.2` or `pc-i440fx-8.2`, when it names one.
    pub machine: Option<String>,
    /// The disks to copy, the boot disk first, the others in the source's order.
    pub disks: Vec<SourceDisk>,
    /// Disks that are not copied, by what the card calls them (a CD-ROM, a disk on other storage).
    pub skipped_disks: Vec<SkippedDisk>,
    pub nics: Vec<SourceNic>,
    /// Every security-group rule the VM template carries.
    pub rules: Vec<SgRule>,
    pub context: Context,
    /// The sha256 of the description's text, in hex.
    pub digest: String,
}

/// How the source boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    Bios,
    Uefi { secure: bool },
}

/// One source disk on Ceph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDisk {
    pub disk_id: u32,
    /// The Ceph pool that holds it.
    pub pool: String,
    /// Its Ceph image in that pool.
    pub image: String,
    pub size_mib: u64,
    pub kind: DiskKind,
    pub bus: Bus,
}

/// What a source disk is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskKind {
    /// A persistent image: the disk is the image itself.
    Persistent { image_name: String },
    /// A non-persistent image: the disk is a copy, often a copy-on-write clone, of the image `base`.
    Clone { base: String, image_name: String },
    /// A volatile disk the outside hypervisor made for this VM alone (`fs` or `swap`).
    Volatile { format: String },
}

/// The bus the guest saw the disk on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bus {
    /// `vd*`: Incus's `io.bus: virtio-blk`.
    VirtioBlk,
    /// `sd*` or `hd*`: Incus's default, virtio-scsi.
    Scsi,
}

/// A disk that is not copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedDisk {
    pub disk_id: u32,
    /// Why, in the card's words.
    pub why: String,
}

/// One NIC of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceNic {
    pub nic_id: u32,
    /// Lowercase, six octets.
    pub mac: String,
    pub ip: Option<Ipv4Addr>,
    pub bridge: String,
    /// The outside hypervisor's network name, for the card.
    pub network: Option<String>,
    /// The security groups the NIC names, in its order.
    pub security_groups: Vec<u32>,
}

/// The direction of a security-group rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound,
    Outbound,
}

/// The protocol of a security-group rule, when it is one an Incus ACL can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    All,
    Tcp,
    Udp,
    Icmp,
    Icmpv6,
    /// IPsec, or a protocol this build does not know: the rule is not carried.
    Other,
}

/// One security-group rule, as the VM template carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SgRule {
    pub group_id: u32,
    pub group_name: String,
    pub direction: Direction,
    pub protocol: Protocol,
    /// The port ranges, `22,8000:8100`, as the source writes them.
    pub range: Option<String>,
    /// The first address of the range the rule names, and how many addresses.
    pub ip: Option<IpAddr>,
    pub size: Option<u64>,
    pub icmp_type: Option<u8>,
    /// The outside hypervisor network the rule names, when it names one.
    pub network_id: Option<u32>,
}

/// What the context CD carried that the seed carries on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// The context says `NETWORK` `YES`: the guest's network comes from the context.
    pub network: bool,
    pub ssh_keys: Vec<String>,
    pub hostname: Option<String>,
    /// The `ETH<n>_*` keys, by `n`.
    pub eth: BTreeMap<u32, EthContext>,
    /// The user data, decoded.
    pub user_data: Option<String>,
    /// The context keys the seed does not carry, by name, sorted. Their values are never kept: some are passwords.
    pub dropped: Vec<String>,
}

/// One NIC's network, as the context gave it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EthContext {
    pub mac: Option<String>,
    pub ip: Option<Ipv4Addr>,
    pub prefix: Option<u8>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<IpAddr>,
    pub search: Vec<String>,
    pub mtu: Option<u32>,
    pub ip6: Option<Ipv6Addr>,
    pub ip6_prefix: Option<u8>,
    pub ip6_gateway: Option<Ipv6Addr>,
}

/// Why a description was refused, with the place it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

fn refuse<T>(why: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError(why.into()))
}

/// One element of the description: its name, its text and its children.
#[derive(Debug, Default)]
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// The trimmed text of the child `name`, when it has some.
    fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|child| child.text.trim())
            .filter(|text| !text.is_empty())
    }
}

/// Reads the description into a tree. Only elements and their text count; attributes, comments and instructions are
/// ignored. A document type declaration is refused, so no entity is ever defined by the description.
fn tree(xml: &str) -> Result<Node, ParseError> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Node> = vec![Node::default()];
    loop {
        let event = reader
            .read_event()
            .map_err(|error| ParseError(format!("the description is not XML: {error}")))?;
        match event {
            Event::Start(start) => {
                if stack.len() > MAX_DEPTH {
                    return refuse("the description nests deeper than a VM description does");
                }
                stack.push(Node {
                    name: start.name().as_ref().to_string(),
                    ..Node::default()
                });
            }
            Event::Empty(start) => {
                let node = Node {
                    name: start.name().as_ref().to_string(),
                    ..Node::default()
                };
                stack.last_mut().expect("the root").children.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().expect("an open element");
                let Some(parent) = stack.last_mut() else {
                    return refuse("the description closes an element it did not open");
                };
                parent.children.push(node);
            }
            Event::Text(text) => {
                stack
                    .last_mut()
                    .expect("the root")
                    .text
                    .push_str(&text.xml10_content());
            }
            Event::CData(data) => {
                stack
                    .last_mut()
                    .expect("the root")
                    .text
                    .push_str(&data.xml10_content());
            }
            Event::GeneralRef(reference) => {
                let resolved = match reference.resolve_char_ref() {
                    Ok(Some(c)) => c,
                    Ok(None) => match reference.into_inner().as_ref() {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "quot" => '"',
                        "apos" => '\'',
                        other => {
                            return refuse(format!(
                                "the description uses the entity &{other}; which XML does not define"
                            ));
                        }
                    },
                    Err(_) => {
                        return refuse(
                            "the description holds a character reference that is not one",
                        );
                    }
                };
                stack.last_mut().expect("the root").text.push(resolved);
            }
            Event::DocType(_) => return refuse("the description declares a document type"),
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) => {}
        }
    }
    if stack.len() != 1 {
        return refuse("the description ends inside an element");
    }
    let mut root = stack.pop().expect("the root");
    let mut elements = root.children.drain(..);
    match (elements.next(), elements.next()) {
        (Some(vm), None) if vm.name == "VM" => Ok(vm),
        _ => refuse("the description is not one <VM> element"),
    }
}

/// Reads one source description.
pub fn parse(xml: &str) -> Result<SourceVm, ParseError> {
    if xml.len() > MAX_SOURCE_BYTES {
        return refuse(format!(
            "the description is longer than {MAX_SOURCE_BYTES} bytes"
        ));
    }
    let vm = tree(xml)?;
    let digest = hex(&Sha256::digest(xml.as_bytes()));
    let id = number::<u64>(vm.text_of("ID"), "ID")?
        .ok_or_else(|| ParseError("ID: the description names no VM id".into()))?;
    let name = vm
        .text_of("NAME")
        .ok_or_else(|| ParseError("NAME: the description names no VM".into()))?
        .to_string();
    let state = state_words(vm.text_of("STATE"), vm.text_of("LCM_STATE"));
    let template = vm
        .child("TEMPLATE")
        .ok_or_else(|| ParseError("TEMPLATE: the description has no template".into()))?;
    let vcpu = number::<u32>(template.text_of("VCPU"), "TEMPLATE/VCPU")?.unwrap_or(1);
    if vcpu == 0 {
        return refuse("TEMPLATE/VCPU: a VM has at least one CPU");
    }
    let memory_mib = number::<u64>(template.text_of("MEMORY"), "TEMPLATE/MEMORY")?
        .filter(|mib| *mib > 0)
        .ok_or_else(|| ParseError("TEMPLATE/MEMORY: the description names no memory".into()))?;
    let memory_max_mib = number::<u64>(template.text_of("MEMORY_MAX"), "TEMPLATE/MEMORY_MAX")?
        .filter(|max| *max > memory_mib);
    let os = template.child("OS");
    let firmware = firmware(os)?;
    let machine = os
        .and_then(|os| os.text_of("MACHINE"))
        .map(|machine| {
            if plain(machine, 64, b"._-") {
                Ok(machine.to_string())
            } else {
                refuse("TEMPLATE/OS/MACHINE: not a machine type")
            }
        })
        .transpose()?;
    let boot_disk = os.and_then(|os| os.text_of("BOOT")).and_then(|boot| {
        boot.split(',')
            .find_map(|part| part.trim().strip_prefix("disk"))
            .and_then(|n| n.parse::<u32>().ok())
    });
    let (disks, skipped_disks) = disks(template, id, boot_disk)?;
    let nics = nics(template)?;
    let rules = rules(template)?;
    let context = context(template.child("CONTEXT"))?;
    Ok(SourceVm {
        id,
        name,
        state,
        vcpu,
        memory_mib,
        memory_max_mib,
        firmware,
        machine,
        disks,
        skipped_disks,
        nics,
        rules,
        context,
        digest,
    })
}

/// The elements [`parse`] reads under each parent; every other element is left out of [`reduce`]'s text.
fn kept(parent: &str, name: &str) -> bool {
    let keys: &[&str] = match parent {
        "VM" => &["ID", "NAME", "STATE", "LCM_STATE", "TEMPLATE"],
        "TEMPLATE" => &[
            "VCPU",
            "MEMORY",
            "MEMORY_MAX",
            "OS",
            "DISK",
            "NIC",
            "SECURITY_GROUP_RULE",
            "CONTEXT",
        ],
        "OS" => &["FIRMWARE", "FIRMWARE_SECURE", "MACHINE", "BOOT"],
        "DISK" => &[
            "DISK_ID",
            "TYPE",
            "TM_MAD",
            "SIZE",
            "TARGET",
            "DEV_PREFIX",
            "IMAGE_ID",
            "SOURCE",
            "IMAGE",
            "CLONE",
            "POOL_NAME",
        ],
        "NIC" => &[
            "NIC_ID",
            "MAC",
            "IP",
            "BRIDGE",
            "SECURITY_GROUPS",
            "NETWORK",
        ],
        "SECURITY_GROUP_RULE" => &[
            "SECURITY_GROUP_ID",
            "SECURITY_GROUP_NAME",
            "RULE_TYPE",
            "PROTOCOL",
            "RANGE",
            "IP",
            "SIZE",
            "ICMP_TYPE",
            "NETWORK_ID",
        ],
        _ => &[],
    };
    keys.contains(&name)
}

/// A context key whose value the seed reads. The other keys [`read_key`] knows are kept by name only.
fn context_value_kept(key: &str) -> bool {
    match eth_key(key) {
        Some((_, field)) => matches!(
            field,
            "MAC"
                | "IP"
                | "MASK"
                | "GATEWAY"
                | "DNS"
                | "SEARCH_DOMAIN"
                | "MTU"
                | "IP6"
                | "IP6_PREFIX_LENGTH"
                | "IP6_GATEWAY"
        ),
        None => read_key(key) && !matches!(key, "DISK_ID" | "TARGET"),
    }
}

/// How [`push_text`] writes one character.
#[derive(Clone, Copy)]
enum Write {
    /// As itself, outside a CDATA section.
    Plain,
    /// As a reference, outside a CDATA section.
    Reference,
    /// Opens a CDATA section and writes it there.
    Open,
    /// As itself, inside the open CDATA section.
    Inside,
    /// Closes the CDATA section and writes it as itself.
    ClosePlain,
    /// Closes the CDATA section and writes it as a reference.
    CloseReference,
}

/// The shortest reference XML has for a character that needs one.
fn reference(c: char) -> &'static str {
    match c {
        '&' => "&amp;",
        '<' => "&lt;",
        '>' => "&gt;",
        _ => "&#13;",
    }
}

/// Element text as the shortest XML that reads back as it, so a reduced text is never longer than the source, which had
/// to write the same characters some valid way.
///
/// Outside a CDATA section `&` and `<` need a reference, and so does the `>` of a `]]>`; inside one, `]]>` cannot occur.
/// A carriage return is a reference anywhere, because a reader turns a literal one into a line feed. Opening a section
/// costs 9 bytes and closing it 3. The cheapest way through the text is found over six states: outside or inside a
/// section, each after zero, one, or two or more `]`.
fn push_text(out: &mut String, text: &str) {
    if !text.contains(['&', '<', '>', '\r']) {
        out.push_str(text);
        return;
    }
    const OPEN: usize = 9;
    const CLOSE: usize = 3;
    const NONE: usize = usize::MAX;
    let chars: Vec<char> = text.chars().collect();
    // States 0..3: outside, after k `]`; 3..6: inside, after k `]`.
    let mut cost = [0, NONE, NONE, NONE, NONE, NONE];
    let mut back: Vec<[(usize, Write); 6]> = Vec::with_capacity(chars.len());
    for &c in &chars {
        let width = c.len_utf8();
        let after = |k: usize| if c == ']' { (k + 1).min(2) } else { 0 };
        let mut next = [NONE; 6];
        let mut from = [(0, Write::Plain); 6];
        let mut offer = |state: usize, total: usize, step: (usize, Write)| {
            if total < next[state] {
                next[state] = total;
                from[state] = step;
            }
        };
        let plain_ok = |k: usize| !matches!(c, '&' | '<' | '\r') && !(c == '>' && k == 2);
        let inside_ok = |k: usize| c != '\r' && !(c == '>' && k == 2);
        let needs_reference = matches!(c, '&' | '<' | '>' | '\r');
        for (state, &paid) in cost.iter().enumerate() {
            if paid == NONE {
                continue;
            }
            if state < 3 {
                let k = state;
                if plain_ok(k) {
                    offer(after(k), paid + width, (state, Write::Plain));
                }
                if needs_reference {
                    offer(0, paid + reference(c).len(), (state, Write::Reference));
                }
                if inside_ok(0) {
                    offer(3 + after(0), paid + OPEN + width, (state, Write::Open));
                }
            } else {
                let k = state - 3;
                if inside_ok(k) {
                    offer(3 + after(k), paid + width, (state, Write::Inside));
                }
                if plain_ok(0) {
                    offer(after(0), paid + CLOSE + width, (state, Write::ClosePlain));
                }
                if needs_reference {
                    offer(
                        0,
                        paid + CLOSE + reference(c).len(),
                        (state, Write::CloseReference),
                    );
                }
            }
        }
        cost = next;
        back.push(from);
    }
    let finish = |state: usize| cost[state].saturating_add(if state >= 3 { CLOSE } else { 0 });
    let mut state = (0..6).min_by_key(|&state| finish(state)).unwrap_or(0);
    let last = state;
    let mut steps = Vec::with_capacity(chars.len());
    for from in back.iter().rev() {
        let (previous, write) = from[state];
        steps.push(write);
        state = previous;
    }
    for (c, write) in chars.iter().zip(steps.iter().rev()) {
        match write {
            Write::Plain | Write::Inside => out.push(*c),
            Write::Reference => out.push_str(reference(*c)),
            Write::Open => {
                out.push_str("<![CDATA[");
                out.push(*c);
            }
            Write::ClosePlain => {
                out.push_str("]]>");
                out.push(*c);
            }
            Write::CloseReference => {
                out.push_str("]]>");
                out.push_str(reference(*c));
            }
        }
    }
    if last >= 3 {
        out.push_str("]]>");
    }
}

fn push_node(out: &mut String, parent: &str, node: &Node, user_data: bool) {
    if parent == "CONTEXT" {
        if !plain(&node.name, 64, b"_") {
            return;
        }
        let text = node.text.trim();
        let hidden = !user_data && matches!(node.name.as_str(), "USER_DATA" | "USERDATA");
        if context_value_kept(&node.name) && !text.is_empty() && !hidden {
            out.push_str(&format!("<{}>", node.name));
            push_text(out, text);
            out.push_str(&format!("</{}>", node.name));
        } else {
            out.push_str(&format!("<{}/>", node.name));
        }
        return;
    }
    if !kept(parent, &node.name) {
        return;
    }
    let mut content = String::new();
    if matches!(
        node.name.as_str(),
        "TEMPLATE" | "OS" | "DISK" | "NIC" | "SECURITY_GROUP_RULE" | "CONTEXT"
    ) {
        for child in &node.children {
            push_node(&mut content, &node.name, child, user_data);
        }
    } else {
        push_text(&mut content, node.text.trim());
    }
    push_element(out, &node.name, &content);
}

/// `<name>content</name>`, or `<name/>` when nothing is left in it, so an element the source wrote self-closed is
/// never written longer.
fn push_element(out: &mut String, name: &str, content: &str) {
    if content.is_empty() {
        out.push_str(&format!("<{name}/>"));
    } else {
        out.push_str(&format!("<{name}>{content}</{name}>"));
    }
}

/// The description cut down to what [`parse`] reads, so a proposal binds and keeps no more of it than the plan uses.
///
/// Every element `parse` reads is kept, in its order, with its text; every other element is left out, with its text
/// (a VNC password, the template's own notes, the outside hypervisor's history). A context key the seed does not
/// carry stays as an empty element, so the card can still name it, but its value is gone: `PASSWORD`,
/// `CRYPTED_PASSWORD` and `START_SCRIPT_BASE64` never reach a proposal. Each text is written in its shortest form, and
/// an element with nothing left in it as `<X/>`, so the reduced text of a well-formed description is never longer than
/// the whole one, and a description under the size limit stays under it. A text that quick-xml accepts but XML does not
/// (a plain `]]>`) can come out longer, because the reduced text is always well-formed. `parse` reads the reduced text
/// as it read the whole, but for the digest, and the reduction of a reduced text is the same text.
pub fn reduce(xml: &str) -> Result<String, ParseError> {
    reduce_to(xml, true)
}

/// The reduced description as it is shown to anyone who lists a proposal: [`reduce`]'s text with the user data left
/// out too (`USER_DATA` and `USERDATA` as empty elements). The proposal keeps, binds and runs the text [`reduce`] gives;
/// this one is only shown.
pub fn reduce_shown(xml: &str) -> Result<String, ParseError> {
    reduce_to(xml, false)
}

fn reduce_to(xml: &str, user_data: bool) -> Result<String, ParseError> {
    if xml.len() > MAX_SOURCE_BYTES {
        return refuse(format!(
            "the description is longer than {MAX_SOURCE_BYTES} bytes"
        ));
    }
    let vm = tree(xml)?;
    let mut content = String::new();
    for child in &vm.children {
        push_node(&mut content, "VM", child, user_data);
    }
    let mut out = String::new();
    push_element(&mut out, "VM", &content);
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn number<T: std::str::FromStr>(text: Option<&str>, place: &str) -> Result<Option<T>, ParseError> {
    text.map(|text| {
        text.parse::<T>().map_err(|_| {
            ParseError(format!(
                "{place}: expected a whole number, not {:?}",
                shorten(text)
            ))
        })
    })
    .transpose()
}

/// At most 40 characters of `text`, for a refusal.
fn shorten(text: &str) -> String {
    text.chars().take(40).collect()
}

/// Whether `text` is 1 to `max` ASCII letters, digits and `extra` bytes.
fn plain(text: &str, max: usize, extra: &[u8]) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || extra.contains(&byte))
}

fn state_words(state: Option<&str>, lcm: Option<&str>) -> Option<&'static str> {
    Some(match (state?, lcm.unwrap_or("0")) {
        ("3", "3") => "running",
        ("3", _) => "busy in the outside hypervisor",
        ("2", _) => "on hold",
        ("4", _) => "stopped",
        ("5", _) => "suspended",
        ("8", _) => "powered off",
        ("9", _) => "undeployed",
        _ => return None,
    })
}

fn firmware(os: Option<&Node>) -> Result<Firmware, ParseError> {
    let Some(os) = os else {
        return Ok(Firmware::Bios);
    };
    let named = os.text_of("FIRMWARE").unwrap_or("");
    let secure = os.text_of("FIRMWARE_SECURE").is_some_and(|secure| {
        secure.eq_ignore_ascii_case("yes") || secure.eq_ignore_ascii_case("true")
    });
    let upper = named.to_ascii_uppercase();
    if named.is_empty() || upper == "BIOS" {
        Ok(Firmware::Bios)
    } else if upper == "UEFI"
        || upper.contains("OVMF")
        || upper.contains("AAVMF")
        || upper.ends_with(".FD")
    {
        Ok(Firmware::Uefi { secure })
    } else {
        refuse(format!(
            "TEMPLATE/OS/FIRMWARE: {:?} is neither BIOS nor a UEFI firmware",
            shorten(named)
        ))
    }
}

/// A Ceph pool or image name: letters, digits, `.`, `_` and `-`, at most 128 characters.
pub fn ceph_name_ok(name: &str) -> bool {
    plain(name, 128, b"._-") && !name.starts_with('-')
}

fn disks(
    template: &Node,
    vm_id: u64,
    boot: Option<u32>,
) -> Result<(Vec<SourceDisk>, Vec<SkippedDisk>), ParseError> {
    let mut disks = Vec::new();
    let mut skipped = Vec::new();
    for (index, disk) in template.all("DISK").enumerate() {
        if index >= MAX_ITEMS {
            return refuse("TEMPLATE/DISK: more disks than a VM description holds");
        }
        let place = |key: &str| format!("TEMPLATE/DISK[{index}]/{key}");
        let disk_id = number::<u32>(disk.text_of("DISK_ID"), &place("DISK_ID"))?
            .ok_or_else(|| ParseError(format!("{}: a disk has no id", place("DISK_ID"))))?;
        let kind_word = disk.text_of("TYPE").unwrap_or("").to_ascii_lowercase();
        if kind_word == "cdrom" {
            skipped.push(SkippedDisk {
                disk_id,
                why: "a CD-ROM".to_string(),
            });
            continue;
        }
        let tm = disk.text_of("TM_MAD").unwrap_or("").to_ascii_lowercase();
        if tm != "ceph" {
            let tm = if tm.is_empty() {
                "unnamed".to_string()
            } else {
                shorten(&tm)
            };
            skipped.push(SkippedDisk {
                disk_id,
                why: format!("on {tm} storage, not Ceph"),
            });
            continue;
        }
        let size_mib = number::<u64>(disk.text_of("SIZE"), &place("SIZE"))?
            .filter(|size| *size > 0)
            .ok_or_else(|| ParseError(format!("{}: the disk has no size", place("SIZE"))))?;
        let bus = match disk
            .text_of("TARGET")
            .or_else(|| disk.text_of("DEV_PREFIX"))
            .unwrap_or("vd")
        {
            target if target.starts_with("vd") => Bus::VirtioBlk,
            _ => Bus::Scsi,
        };
        let (pool, image, kind) = if disk.text_of("IMAGE_ID").is_some() {
            let source = disk.text_of("SOURCE").ok_or_else(|| {
                ParseError(format!(
                    "{}: an image disk names no Ceph image",
                    place("SOURCE")
                ))
            })?;
            let Some((pool, base)) = source.split_once('/') else {
                return refuse(format!("{}: expected <pool>/<image>", place("SOURCE")));
            };
            let image_name = disk.text_of("IMAGE").unwrap_or(base).to_string();
            let clone = disk
                .text_of("CLONE")
                .is_some_and(|clone| clone.eq_ignore_ascii_case("yes"));
            if clone {
                (
                    pool.to_string(),
                    format!("{base}-{vm_id}-{disk_id}"),
                    DiskKind::Clone {
                        base: base.to_string(),
                        image_name,
                    },
                )
            } else {
                (
                    pool.to_string(),
                    base.to_string(),
                    DiskKind::Persistent { image_name },
                )
            }
        } else {
            let pool = disk.text_of("POOL_NAME").ok_or_else(|| {
                ParseError(format!(
                    "{}: a volatile disk names no Ceph pool",
                    place("POOL_NAME")
                ))
            })?;
            let format = if kind_word.is_empty() {
                "fs".to_string()
            } else {
                kind_word.clone()
            };
            if !plain(&format, 16, b"_-") {
                return refuse(format!("{}: not a disk type", place("TYPE")));
            }
            (
                pool.to_string(),
                format!("one-sys-{vm_id}-{disk_id}"),
                DiskKind::Volatile { format },
            )
        };
        if !ceph_name_ok(&pool) || !ceph_name_ok(&image) {
            return refuse(format!(
                "{}: the Ceph pool or image name holds characters a Ceph name does not",
                place("SOURCE")
            ));
        }
        disks.push(SourceDisk {
            disk_id,
            pool,
            image,
            size_mib,
            kind,
            bus,
        });
    }
    let mut ids: Vec<u32> = disks
        .iter()
        .map(|disk| disk.disk_id)
        .chain(skipped.iter().map(|d| d.disk_id))
        .collect();
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return refuse("TEMPLATE/DISK: two disks have the same id");
    }
    let first = boot
        .and_then(|id| disks.iter().position(|disk| disk.disk_id == id))
        .or_else(|| {
            disks
                .iter()
                .enumerate()
                .min_by_key(|(_, disk)| disk.disk_id)
                .map(|(at, _)| at)
        });
    if let Some(at) = first {
        let boot_disk = disks.remove(at);
        disks.insert(0, boot_disk);
    }
    Ok((disks, skipped))
}

/// `02:00:C0:00:02:0A` as `02:00:c0:00:02:0a`; `None` for anything that is not six hex octets.
pub fn normalize_mac(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    (parts.len() == 6
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit())))
    .then(|| parts.join(":").to_ascii_lowercase())
}

/// A bridge name: letters, digits, `.`, `_` and `-`, at most 15 characters (a Linux interface name).
pub fn bridge_ok(name: &str) -> bool {
    plain(name, 15, b"._-")
}

fn nics(template: &Node) -> Result<Vec<SourceNic>, ParseError> {
    let mut nics = Vec::new();
    for (index, nic) in template.all("NIC").enumerate() {
        if index >= MAX_ITEMS {
            return refuse("TEMPLATE/NIC: more NICs than a VM description holds");
        }
        let place = |key: &str| format!("TEMPLATE/NIC[{index}]/{key}");
        let nic_id =
            number::<u32>(nic.text_of("NIC_ID"), &place("NIC_ID"))?.unwrap_or(index as u32);
        let mac = nic
            .text_of("MAC")
            .and_then(normalize_mac)
            .ok_or_else(|| ParseError(format!("{}: expected six hex octets", place("MAC"))))?;
        let ip = nic
            .text_of("IP")
            .map(|ip| {
                ip.parse::<Ipv4Addr>()
                    .map_err(|_| ParseError(format!("{}: not an IPv4 address", place("IP"))))
            })
            .transpose()?;
        let bridge = nic
            .text_of("BRIDGE")
            .ok_or_else(|| ParseError(format!("{}: the NIC names no bridge", place("BRIDGE"))))?;
        if !bridge_ok(bridge) {
            return refuse(format!("{}: not a bridge name", place("BRIDGE")));
        }
        let security_groups = match nic.text_of("SECURITY_GROUPS") {
            None => Vec::new(),
            Some(list) => list
                .split(',')
                .map(|id| {
                    id.trim().parse::<u32>().map_err(|_| {
                        ParseError(format!("{}: expected group ids", place("SECURITY_GROUPS")))
                    })
                })
                .collect::<Result<_, _>>()?,
        };
        nics.push(SourceNic {
            nic_id,
            mac,
            ip,
            bridge: bridge.to_string(),
            network: nic.text_of("NETWORK").map(str::to_string),
            security_groups,
        });
    }
    nics.sort_by_key(|nic| nic.nic_id);
    if nics
        .windows(2)
        .any(|pair| pair[0].nic_id == pair[1].nic_id || pair[0].mac == pair[1].mac)
    {
        return refuse("TEMPLATE/NIC: two NICs have the same id or MAC");
    }
    Ok(nics)
}

fn rules(template: &Node) -> Result<Vec<SgRule>, ParseError> {
    let mut rules = Vec::new();
    for (index, rule) in template.all("SECURITY_GROUP_RULE").enumerate() {
        if index >= MAX_ITEMS * 4 {
            return refuse("TEMPLATE/SECURITY_GROUP_RULE: more rules than this build carries");
        }
        let place = |key: &str| format!("TEMPLATE/SECURITY_GROUP_RULE[{index}]/{key}");
        let group_id = number::<u32>(
            rule.text_of("SECURITY_GROUP_ID"),
            &place("SECURITY_GROUP_ID"),
        )?
        .ok_or_else(|| {
            ParseError(format!(
                "{}: the rule names no group",
                place("SECURITY_GROUP_ID")
            ))
        })?;
        let direction = match rule
            .text_of("RULE_TYPE")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("inbound") => Direction::Inbound,
            Some("outbound") => Direction::Outbound,
            _ => {
                return refuse(format!(
                    "{}: expected inbound or outbound",
                    place("RULE_TYPE")
                ));
            }
        };
        let protocol = match rule
            .text_of("PROTOCOL")
            .map(str::to_ascii_uppercase)
            .as_deref()
            .unwrap_or("ALL")
        {
            "ALL" => Protocol::All,
            "TCP" => Protocol::Tcp,
            "UDP" => Protocol::Udp,
            "ICMP" => Protocol::Icmp,
            "ICMPV6" => Protocol::Icmpv6,
            _ => Protocol::Other,
        };
        let range = rule
            .text_of("RANGE")
            .map(|range| {
                if plain(range, 128, b",:") && range.split(',').all(range_ok) {
                    Ok(range.to_string())
                } else {
                    refuse(format!(
                        "{}: expected ports such as 22,8000:8100",
                        place("RANGE")
                    ))
                }
            })
            .transpose()?;
        let ip = rule
            .text_of("IP")
            .map(|ip| {
                ip.parse::<IpAddr>()
                    .map_err(|_| ParseError(format!("{}: not an address", place("IP"))))
            })
            .transpose()?;
        let size = number::<u64>(rule.text_of("SIZE"), &place("SIZE"))?;
        let icmp_type = number::<u8>(rule.text_of("ICMP_TYPE"), &place("ICMP_TYPE"))?;
        let network_id = number::<u32>(rule.text_of("NETWORK_ID"), &place("NETWORK_ID"))?;
        rules.push(SgRule {
            group_id,
            group_name: rule
                .text_of("SECURITY_GROUP_NAME")
                .unwrap_or("")
                .to_string(),
            direction,
            protocol,
            range,
            ip,
            size,
            icmp_type,
            network_id,
        });
    }
    Ok(rules)
}

fn range_ok(part: &str) -> bool {
    let port = |text: &str| text.parse::<u16>().is_ok_and(|port| port > 0);
    match part.split_once(':') {
        Some((low, high)) => {
            port(low) && port(high) && low.parse::<u16>().ok() <= high.parse::<u16>().ok()
        }
        None => port(part),
    }
}

/// The context keys the seed reads; every other key is named as not carried.
fn read_key(key: &str) -> bool {
    matches!(
        key,
        "NETWORK"
            | "SSH_PUBLIC_KEY"
            | "SET_HOSTNAME"
            | "HOSTNAME"
            | "USER_DATA"
            | "USERDATA"
            | "USERDATA_ENCODING"
            | "USER_DATA_ENCODING"
            | "DISK_ID"
            | "TARGET"
    ) || eth_key(key).is_some()
}

/// `ETH3_IP` as `(3, "IP")`.
fn eth_key(key: &str) -> Option<(u32, &str)> {
    let rest = key.strip_prefix("ETH")?;
    let (n, field) = rest.split_once('_')?;
    Some((n.parse().ok()?, field))
}

/// A dotted mask, or a prefix length, as a prefix length.
fn prefix_of(mask: &str) -> Option<u8> {
    if let Ok(length) = mask.parse::<u8>() {
        return (length <= 32).then_some(length);
    }
    let bits = u32::from(mask.parse::<Ipv4Addr>().ok()?);
    let length = bits.leading_ones();
    (bits.checked_shl(length).unwrap_or(0) == 0).then_some(length as u8)
}

/// A host name: DNS labels of letters, digits and `-`, at most 253 characters.
pub fn hostname_ok(name: &str) -> bool {
    name.len() <= 253
        && name
            .split('.')
            .all(|label| plain(label, 63, b"-") && !label.starts_with('-') && !label.ends_with('-'))
}

fn context(node: Option<&Node>) -> Result<Context, ParseError> {
    let mut context = Context::default();
    let Some(node) = node else {
        return Ok(context);
    };
    let value = |key: &str| node.text_of(key);
    context.network = value("NETWORK").is_some_and(|yes| yes.eq_ignore_ascii_case("yes"));
    if let Some(keys) = value("SSH_PUBLIC_KEY") {
        for line in keys.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if line.len() > 16 * 1024 || line.chars().any(char::is_control) {
                return refuse("TEMPLATE/CONTEXT/SSH_PUBLIC_KEY: a key line is not one key");
            }
            context.ssh_keys.push(line.to_string());
        }
    }
    context.hostname = value("SET_HOSTNAME")
        .or_else(|| value("HOSTNAME"))
        .filter(|name| hostname_ok(name))
        .map(str::to_string);
    let encoded = value("USERDATA_ENCODING")
        .or_else(|| value("USER_DATA_ENCODING"))
        .is_some_and(|encoding| encoding.eq_ignore_ascii_case("base64"));
    if let Some(data) = node
        .child("USER_DATA")
        .or_else(|| node.child("USERDATA"))
        .map(|child| child.text.as_str())
        .filter(|data| !data.trim().is_empty())
    {
        let text = if encoded {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.split_whitespace().collect::<String>())
                .map_err(|_| ParseError("TEMPLATE/CONTEXT/USER_DATA: not base64".into()))?;
            String::from_utf8(bytes)
                .map_err(|_| ParseError("TEMPLATE/CONTEXT/USER_DATA: not text".into()))?
        } else {
            data.to_string()
        };
        context.user_data = Some(text.trim().to_string());
    }
    let mut dropped = Vec::new();
    for child in &node.children {
        if !read_key(&child.name) {
            if plain(&child.name, 64, b"_") {
                dropped.push(child.name.clone());
            }
            continue;
        }
        let Some((n, field)) = eth_key(&child.name) else {
            continue;
        };
        let text = child.text.trim();
        if text.is_empty() {
            continue;
        }
        let place = format!("TEMPLATE/CONTEXT/{}", child.name);
        let bad = |what: &str| ParseError(format!("{place}: not {what}"));
        let eth = context.eth.entry(n).or_default();
        match field {
            "MAC" => eth.mac = Some(normalize_mac(text).ok_or_else(|| bad("a MAC"))?),
            "IP" => eth.ip = Some(text.parse().map_err(|_| bad("an IPv4 address"))?),
            "MASK" => eth.prefix = Some(prefix_of(text).ok_or_else(|| bad("a network mask"))?),
            "GATEWAY" => eth.gateway = Some(text.parse().map_err(|_| bad("an IPv4 address"))?),
            "DNS" => {
                eth.dns = text
                    .split_whitespace()
                    .map(|server| {
                        server
                            .parse::<IpAddr>()
                            .map_err(|_| bad("name server addresses"))
                    })
                    .collect::<Result<_, _>>()?;
            }
            "SEARCH_DOMAIN" => {
                eth.search = text.split_whitespace().map(str::to_string).collect();
                if !eth.search.iter().all(|domain| hostname_ok(domain)) {
                    return Err(bad("search domains"));
                }
            }
            "MTU" => {
                eth.mtu = Some(
                    text.parse::<u32>()
                        .ok()
                        .filter(|mtu| (576..=65_535).contains(mtu))
                        .ok_or_else(|| bad("an MTU"))?,
                );
            }
            "IP6" => eth.ip6 = Some(text.parse().map_err(|_| bad("an IPv6 address"))?),
            "IP6_PREFIX_LENGTH" => {
                eth.ip6_prefix = Some(
                    text.parse::<u8>()
                        .ok()
                        .filter(|length| *length <= 128)
                        .ok_or_else(|| bad("a prefix length"))?,
                );
            }
            "IP6_GATEWAY" => {
                eth.ip6_gateway = Some(text.parse().map_err(|_| bad("an IPv6 address"))?)
            }
            // Other ETH keys (the network's name, its method, its IPv6 ULA) are the outside hypervisor's own.
            _ => {}
        }
    }
    dropped.sort();
    dropped.dedup();
    context.dropped = dropped;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mask_reads_as_its_prefix_length() {
        assert_eq!(prefix_of("255.255.255.0"), Some(24));
        assert_eq!(prefix_of("255.255.0.0"), Some(16));
        assert_eq!(prefix_of("0.0.0.0"), Some(0));
        assert_eq!(prefix_of("24"), Some(24));
        assert_eq!(prefix_of("255.0.255.0"), None);
        assert_eq!(prefix_of("33"), None);
    }

    #[test]
    fn a_port_range_holds_ports_low_to_high() {
        assert!(range_ok("22"));
        assert!(range_ok("8000:8100"));
        assert!(!range_ok("0"));
        assert!(!range_ok("8100:8000"));
        assert!(!range_ok("70000"));
    }

    #[test]
    fn a_mac_is_six_hex_octets() {
        assert_eq!(
            normalize_mac("02:00:C0:00:02:0A").as_deref(),
            Some("02:00:c0:00:02:0a")
        );
        assert_eq!(normalize_mac("02:00:c0:00:02"), None);
        assert_eq!(normalize_mac("02:00:c0:00:02:zz"), None);
    }
}
