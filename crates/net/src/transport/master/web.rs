//! Browser transport for the master relay: no QUIC, no threads, no server.
//!
//! The page owns the WebRTC links (see `web/rtc.js`) and calls into this
//! module on a short timer: it hands over what arrived, and takes back what
//! must be sent. Everything else is the same state machines the native
//! session runs, stepped by `tick` instead of by a tokio loop.
//!
//! One tab hosts: it runs the room service ([`ServiceState`]) in-process and
//! its own session talks to that service directly. Other tabs join over a
//! single link (member 0 on their side) that carries the same control frames,
//! gameplay datagrams and bootstrap blobs the QUIC relay would.

use std::cell::RefCell;

use super::service::{ServiceState, normalize_player_name};
use super::*;

/// What a message carries on its link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Reliable and ordered: framed [`ControlFrame`]s (lobby, requests, relays).
    Control = 0,
    /// Unreliable: gameplay datagrams.
    Data = 1,
    /// Reliable and ordered, large: bootstrap blobs.
    Bulk = 2,
}

impl Lane {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Control),
            1 => Some(Self::Data),
            2 => Some(Self::Bulk),
            _ => None,
        }
    }
}

type Msg = (Lane, Vec<u8>);

/// The host's own session is connection 0 of its service; real links are
/// numbered from 1 by the page.
const LOCAL_HOST: u64 = 0;
const LOCAL_BROWSER_BASE: u64 = 1 << 32;
/// The link a joining tab keeps to its host.
const HOST_LINK: u32 = 0;
const TICK_BUDGET: usize = 512;

fn encode_control(frame: &ControlFrame) -> std::result::Result<Vec<u8>, String> {
    encode_stream_frame(frame).map_err(|error| error.to_string())
}

fn decode_control(bytes: &[u8]) -> std::result::Result<ControlFrame, String> {
    let header: [u8; 4] = bytes
        .get(..4)
        .and_then(|head| head.try_into().ok())
        .ok_or("control frame shorter than its header")?;
    let len = stream_frame_len(header).map_err(|error| error.to_string())?;
    let body = bytes
        .get(4..4 + len)
        .ok_or("control frame shorter than its length")?;
    decode_stream_payload(body).map_err(|error| error.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wire {
    /// The room service in this tab, as connection `id`.
    Local(u64),
    /// The link to the host.
    Link,
}

struct TickCtx {
    now: TickInstant,
    wire: Wire,
}

trait Session {
    /// Advance by one step. `true` once the session is over.
    fn tick(&mut self, ctx: &TickCtx, inbox: &mut VecDeque<Msg>, out: &mut Vec<Msg>) -> bool;
}

struct Slot {
    session: Box<dyn Session>,
    inbox: VecDeque<Msg>,
    local_id: u64,
}

struct Hub {
    service: ServiceState,
    links: HashSet<u32>,
    out: Vec<(u32, Lane, Vec<u8>)>,
    host: Option<Slot>,
    join: Option<Slot>,
    browsers: Vec<Slot>,
    next_browser: u64,
    attached: HashSet<u64>,
}

thread_local! {
    static HUB: RefCell<Hub> = RefCell::new(Hub {
        service: ServiceState::default(),
        links: HashSet::new(),
        out: Vec::new(),
        host: None,
        join: None,
        browsers: Vec::new(),
        next_browser: LOCAL_BROWSER_BASE,
        attached: HashSet::new(),
    });
}

impl Hub {
    fn guest_link_open(&self) -> bool {
        self.links.contains(&HOST_LINK)
    }

    fn slot_for(&mut self, conn: u64) -> Option<&mut Slot> {
        if conn == LOCAL_HOST {
            return self.host.as_mut();
        }
        self.browsers.iter_mut().find(|slot| slot.local_id == conn)
    }

    /// Hand a message to a connection of the service: a local session or a link.
    fn deliver(&mut self, conn: u64, lane: Lane, bytes: Vec<u8>) {
        if conn == LOCAL_HOST || conn >= LOCAL_BROWSER_BASE {
            if let Some(slot) = self.slot_for(conn) {
                slot.inbox.push_back((lane, bytes));
            }
        } else if let Ok(link) = u32::try_from(conn) {
            self.out.push((link, lane, bytes));
        }
    }

    fn deliver_frames(&mut self, frames: Vec<(u64, ControlFrame)>) {
        for (conn, frame) in frames {
            if let Ok(bytes) = encode_control(&frame) {
                self.deliver(conn, Lane::Control, bytes);
            }
        }
    }

    fn disconnect(&mut self, conn: u64) {
        self.attached.remove(&conn);
        let publishes = self.service.disconnect(conn);
        self.deliver_frames(publishes);
    }

    /// One message from connection `conn` of the service.
    fn serve(&mut self, conn: u64, lane: Lane, bytes: Vec<u8>) {
        match lane {
            Lane::Control => {
                let frame = match decode_control(&bytes) {
                    Ok(frame) => frame,
                    Err(error) => {
                        diag::warn!(Net, "web master: bad control frame from {conn}: {error}");
                        return;
                    }
                };
                match frame {
                    ControlFrame::Hello(hello) => {
                        if hello.protocol_version != master_protocol::PROTOCOL_VERSION
                            || hello.game_protocol != crate::PROTOCOL_VERSION
                        {
                            diag::warn!(Net, "web master: {conn} runs another version");
                            self.disconnect(conn);
                            return;
                        }
                        let name = normalize_player_name(&hello.player_name);
                        if let Some(peer) = self.service.peers.get_mut(&conn) {
                            peer.player_name = name;
                        } else {
                            self.service.attach_peer(conn, name);
                        }
                        self.attached.insert(conn);
                    }
                    ControlFrame::Request(request) => {
                        if !self.attached.contains(&conn) {
                            return;
                        }
                        let outcome = self.service.request(conn, request);
                        let mut frames = outcome.publishes;
                        frames.push((conn, ControlFrame::Response(outcome.response)));
                        self.deliver_frames(frames);
                    }
                    ControlFrame::Relay(bytes) => {
                        let routed = decode_relay_stream(&bytes)
                            .map_err(|error| error.to_string())
                            .and_then(|relay| {
                                self.service
                                    .route_stream(conn, &relay)
                                    .map_err(|error| error.to_string())
                            });
                        match routed {
                            Ok((target, outgoing)) => {
                                if let Ok(frame) = encode_control(&ControlFrame::Relay(outgoing)) {
                                    self.deliver(target, Lane::Control, frame);
                                }
                            }
                            Err(error) => diag::warn!(Net, "web master: relay refused: {error}"),
                        }
                    }
                    _ => diag::warn!(Net, "web master: {conn} sent a non-request frame"),
                }
            }
            Lane::Data => {
                let routed = decode_relay(&bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|relay| {
                        self.service
                            .route_datagram(conn, &relay)
                            .map_err(|error| error.to_string())
                    });
                if let Ok((target, outgoing)) = routed {
                    self.deliver(target, Lane::Data, outgoing);
                }
            }
            Lane::Bulk => {
                let routed = decode_relay_stream(&bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|relay| {
                        self.service
                            .route_stream(conn, &relay)
                            .map_err(|error| error.to_string())
                    });
                match routed {
                    Ok((target, outgoing)) => self.deliver(target, Lane::Bulk, outgoing),
                    Err(error) => diag::warn!(Net, "web master: bootstrap refused: {error}"),
                }
            }
        }
    }

    fn tick(&mut self) -> Vec<(u32, Lane, Vec<u8>)> {
        let now = TickInstant::now();
        let guest = self.guest_link_open();
        // Sessions first: whatever they send is served in the same call.
        let mut sent: Vec<(u64, bool, Msg)> = Vec::new();
        let mut finished_host = false;
        if let Some(slot) = self.host.as_mut() {
            let mut out = Vec::new();
            let ctx = TickCtx {
                now,
                wire: Wire::Local(LOCAL_HOST),
            };
            finished_host = slot.session.tick(&ctx, &mut slot.inbox, &mut out);
            sent.extend(out.into_iter().map(|msg| (LOCAL_HOST, false, msg)));
        }
        if finished_host {
            self.host = None;
            self.disconnect(LOCAL_HOST);
        }
        let mut finished_join = false;
        if let Some(slot) = self.join.as_mut() {
            let mut out = Vec::new();
            let ctx = TickCtx {
                now,
                wire: Wire::Link,
            };
            finished_join = slot.session.tick(&ctx, &mut slot.inbox, &mut out);
            sent.extend(out.into_iter().map(|msg| (0, true, msg)));
        }
        if finished_join {
            self.join = None;
        }
        let mut closed = Vec::new();
        for slot in &mut self.browsers {
            let mut out = Vec::new();
            let wire = if guest {
                Wire::Link
            } else {
                Wire::Local(slot.local_id)
            };
            let ctx = TickCtx { now, wire };
            if slot.session.tick(&ctx, &mut slot.inbox, &mut out) {
                closed.push(slot.local_id);
            }
            sent.extend(
                out.into_iter()
                    .map(|msg| (slot.local_id, guest, msg)),
            );
        }
        for id in closed {
            self.browsers.retain(|slot| slot.local_id != id);
            self.disconnect(id);
        }
        for (conn, over_link, (lane, bytes)) in sent {
            if over_link {
                self.out.push((HOST_LINK, lane, bytes));
            } else {
                self.serve(conn, lane, bytes);
            }
        }
        std::mem::take(&mut self.out)
    }
}

/// Whether sessions run in the page loop rather than on their own thread.
pub(super) fn inline() -> bool {
    cfg!(target_arch = "wasm32") || forced()
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn forced() -> bool {
    FORCED.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn forced() -> bool {
    false
}

// --- entry points for the page -------------------------------------------------

/// A link finished its handshake and both channels are open.
pub fn on_open(member: u32) {
    HUB.with(|hub| {
        hub.borrow_mut().links.insert(member);
    });
}

/// A link went away.
pub fn on_close(member: u32) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        hub.links.remove(&member);
        if member != HOST_LINK {
            hub.disconnect(u64::from(member));
        }
    });
}

/// A message arrived on a link.
pub fn on_message(member: u32, lane: Lane, bytes: Vec<u8>) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        if member == HOST_LINK {
            if let Some(slot) = hub.join.as_mut() {
                slot.inbox.push_back((lane, bytes));
            } else {
                for slot in &mut hub.browsers {
                    slot.inbox.push_back((lane, bytes.clone()));
                }
            }
        } else {
            hub.serve(u64::from(member), lane, bytes);
        }
    });
}

/// Step every session and the room service; returns what the page must send.
pub fn tick() -> Vec<(u32, Lane, Vec<u8>)> {
    HUB.with(|hub| hub.borrow_mut().tick())
}

/// Whether a link to the host of a room is open on this tab.
pub fn host_link_open() -> bool {
    HUB.with(|hub| hub.borrow().guest_link_open())
}

/// How many guests are connected to the room service of this tab.
pub fn guest_count() -> usize {
    HUB.with(|hub| hub.borrow().links.iter().filter(|id| **id != HOST_LINK).count())
}

// --- the match session ---------------------------------------------------------

pub(super) fn spawn_session(
    kind: SessionKind,
    ctx: WorkerCtx,
) -> std::result::Result<(), TransportFault> {
    let is_host = matches!(kind, SessionKind::Host(_));
    let role = ctx.role;
    if !is_host && !host_link_open() {
        return Err(TransportFault::new(
            "transport",
            role,
            "not connected to a host: open the Multiplayer panel and paste an invite first",
        ));
    }
    let session = MatchSession::new(kind, ctx);
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        let slot = Slot {
            session: Box::new(session),
            inbox: VecDeque::new(),
            local_id: if is_host { LOCAL_HOST } else { HOST_LINK.into() },
        };
        if is_host {
            hub.host = Some(slot);
        } else {
            hub.join = Some(slot);
        }
    });
    Ok(())
}

struct MatchSession {
    role: &'static str,
    is_host: bool,
    hello: Option<ControlHello>,
    state: Arc<Mutex<MasterBridgeState>>,
    commands: Arc<Mutex<Vec<QueuedCommand>>>,
    installed_load: Arc<Mutex<Option<frame::LocalLoadKey>>>,
    mailbox: RelayMailbox,
    bootstrap: Arc<BootstrapLane>,
    cancel: CancellationToken,
    close: CancellationToken,
    facts: Arc<Mutex<Vec<MasterLifecycleFact>>>,
    identity: SessionIdentity,
    fallback_identity: SessionIdentity,
    control_tx: ControlSender,
    control_rx: tokio::sync::mpsc::Receiver<QueuedControl>,
    bootstrap_jobs: BootstrapSender,
    bootstrap_rx: tokio::sync::mpsc::Receiver<BootstrapJob>,
    prepared_tx: tokio::sync::mpsc::Sender<HostMatchEvent>,
    prepared_rx: tokio::sync::mpsc::Receiver<HostMatchEvent>,
    request_id: u64,
    session: SessionCore,
    host_match: HostMatchCore,
    members: HashSet<MemberId>,
    skip_voters: HashSet<MemberId>,
    last_view: Option<RoomView>,
    started_epoch: u32,
    leave_request: Option<u64>,
    leave_at: Option<TickInstant>,
    closing: bool,
    last_watchdog: TickInstant,
    reassemblers: HashMap<MemberId, (Reassembler, Instant)>,
    fragmenter: Fragmenter,
    pending: VecDeque<(MemberId, Vec<u8>, TickInstant)>,
    failed: bool,
}

impl MatchSession {
    fn new(kind: SessionKind, ctx: WorkerCtx) -> Self {
        let WorkerCtx {
            player_name,
            state,
            commands,
            outcomes,
            installed_load,
            mailbox,
            bootstrap,
            cancel,
            close,
            facts,
            identity,
            role,
        } = ctx;
        let (is_host, create, join) = match kind {
            SessionKind::Host(config) => (true, Some(config), None),
            SessionKind::Join(config) => (false, None, Some(config)),
        };
        let (tx, control_rx) =
            tokio::sync::mpsc::channel::<QueuedControl>(limits::values().queue_items);
        let control_tx = ControlSender {
            tx,
            pending: Mutex::new(HashMap::new()),
            outcomes,
            bytes: Arc::new(tokio::sync::Semaphore::new(limits::values().queue_bytes)),
        };
        let (bootstrap_tx, bootstrap_rx) =
            tokio::sync::mpsc::channel::<BootstrapJob>(MAX_CONCURRENT_BOOTSTRAP as usize);
        let bootstrap_jobs = BootstrapSender {
            tx: bootstrap_tx,
            bytes: Arc::new(tokio::sync::Semaphore::new(limits::values().queue_bytes)),
        };
        let (prepared_tx, prepared_rx) =
            tokio::sync::mpsc::channel::<HostMatchEvent>(MAX_CONCURRENT_BOOTSTRAP as usize);
        let mut request_id = 1_u64;
        let first = if let Some(config) = create.as_ref() {
            ControlRequest {
                request_id: take_id(&mut request_id),
                body: RequestBody::CreateRoom {
                    name: config.name.clone(),
                    map: config.map.clone(),
                    mode: config.mode.clone(),
                    max_players: config.max_players,
                    requires: config.requires,
                    available: config.have,
                    password: config.password.clone(),
                },
            }
        } else {
            let config = join.as_ref().expect("join config");
            ControlRequest {
                request_id: take_id(&mut request_id),
                body: RequestBody::JoinRoom {
                    room_id: config.advert_id,
                    password: config.password.clone(),
                    have: config.have,
                },
            }
        };
        let mut this = Self {
            role,
            is_host,
            hello: Some(ControlHello {
                protocol_version: master_protocol::PROTOCOL_VERSION,
                game_protocol: crate::PROTOCOL_VERSION,
                role: if is_host {
                    EndpointRole::Host
                } else {
                    EndpointRole::Join
                },
                build: endpoint_build(),
                player_name,
            }),
            state,
            commands,
            installed_load,
            mailbox,
            bootstrap,
            cancel,
            close,
            facts,
            identity,
            fallback_identity: identity,
            control_tx,
            control_rx,
            bootstrap_jobs,
            bootstrap_rx,
            prepared_tx,
            prepared_rx,
            request_id,
            session: SessionCore::for_connection(identity.room_id.0, identity.attempt_id),
            host_match: HostMatchCore::default(),
            members: HashSet::new(),
            skip_voters: HashSet::new(),
            last_view: None,
            started_epoch: 0,
            leave_request: None,
            leave_at: None,
            closing: false,
            last_watchdog: TickInstant::now(),
            reassemblers: HashMap::new(),
            fragmenter: relay_fragmenter(),
            pending: VecDeque::new(),
            failed: false,
        };
        if let Err(error) = enqueue_control(&this.control_tx, ControlFrame::Request(first), role) {
            publish_failed(&this.state, this.fallback_identity, error);
            this.failed = true;
        }
        diag::info!(
            Net,
            "web master {role} session begin attempt={} build={}",
            identity.attempt_id,
            endpoint_build()
        );
        this
    }

    fn step(
        &mut self,
        ctx: &TickCtx,
        inbox: &mut VecDeque<Msg>,
        out: &mut Vec<Msg>,
    ) -> std::result::Result<bool, TransportFault> {
        let role = self.role;
        let is_host = self.is_host;
        if let Some(hello) = self.hello.take() {
            out.push((
                Lane::Control,
                encode_control(&ControlFrame::Hello(hello))
                    .map_err(|error| TransportFault::new("hello", role, error))?,
            ));
        }
        if self.cancel.is_cancelled() {
            return Ok(true);
        }
        let bootstrap = Arc::clone(&self.bootstrap);
        let map_ready = &bootstrap.host_map_ready;
        pump_local_queues(
            self.identity.match_key(),
            &bootstrap,
            &self.control_tx,
            &self.bootstrap_jobs,
            role,
            is_host,
            &mut self.request_id,
            map_ready,
        )?;
        self.mailbox
            .check_control_deadlines()
            .map_err(|error| TransportFault::new("relay_queue", role, error))?;
        for (member_id, payload) in self.mailbox.take_control_outbound() {
            let envelope = if is_host {
                RelayDatagram::HostToMember {
                    member_id,
                    payload: &payload,
                }
            } else {
                RelayDatagram::ClientToHost(&payload)
            };
            let bytes = encode_relay_stream(envelope)
                .map_err(|error| TransportFault::new("control_encode", role, error.to_string()))?;
            enqueue_control(&self.control_tx, ControlFrame::Relay(bytes), role)?;
        }
        for command in take_commands(&self.commands, role)? {
            match handle_command(
                command,
                &self.installed_load,
                &self.control_tx,
                &mut self.request_id,
                &self.facts,
                &mut self.host_match,
                map_ready,
                role,
                is_host,
                self.closing,
            )? {
                CommandEffect::Continue => {}
                CommandEffect::BeginClose { request_id } => {
                    self.closing = true;
                    self.leave_request = Some(request_id);
                    self.leave_at = Some(ctx.now + limits::values().shutdown);
                }
            }
        }
        if self.close.is_cancelled() && !self.closing {
            let id = take_id(&mut self.request_id);
            let body = if is_host {
                RequestBody::CloseRoom
            } else {
                RequestBody::LeaveRoom
            };
            enqueue_control(
                &self.control_tx,
                ControlFrame::Request(ControlRequest {
                    request_id: id,
                    body,
                }),
                role,
            )?;
            self.closing = true;
            self.leave_request = Some(id);
            self.leave_at = Some(ctx.now + limits::values().shutdown);
        }
        if self.leave_at.is_some_and(|at| ctx.now >= at) {
            diag::warn!(Net, "web master {role} closed: host did not confirm in time");
            return Ok(true);
        }
        while let Ok(event) = self.prepared_rx.try_recv() {
            let applied = apply_host(&mut self.host_match, event);
            execute_host_match_effects(
                applied.effects,
                map_ready,
                &self.facts,
                &self.control_tx,
                &mut self.request_id,
                role,
            )?;
        }
        if is_host && ctx.now.duration_since(self.last_watchdog) >= WATCHDOG_INTERVAL {
            self.last_watchdog = ctx.now;
            let tick = apply_host(
                &mut self.host_match,
                HostMatchEvent::Tick { now_ms: now_ms() },
            );
            execute_host_match_effects(
                tick.effects,
                map_ready,
                &self.facts,
                &self.control_tx,
                &mut self.request_id,
                role,
            )?;
        }

        let fact_limit = limits::values()
            .queue_items
            .max(master_protocol::MAX_SESSION_MEMBERS as usize + 1);
        let mut budget = TICK_BUDGET;
        while budget > 0 {
            budget -= 1;
            let Some((lane, _)) = inbox.front() else {
                break;
            };
            if *lane == Lane::Control
                && (self.facts.lock().expect("master facts poisoned").len() >= fact_limit
                    || !self.mailbox.control_inbound_has_capacity())
            {
                break;
            }
            let (lane, bytes) = inbox.pop_front().expect("front was present");
            match lane {
                Lane::Control => {
                    let frame = decode_control(&bytes)
                        .map_err(|error| TransportFault::new("control_read", role, error))?;
                    if let ControlFrame::Relay(bytes) = frame {
                        let ingress = match decode_relay_stream(&bytes) {
                            Ok(RelayDatagram::ServiceToHost { member_id, payload }) if is_host => {
                                Some((member_id, payload.to_vec()))
                            }
                            Ok(RelayDatagram::ServiceToMember(payload)) if !is_host => {
                                Some((MemberId([0; 16]), payload.to_vec()))
                            }
                            _ => None,
                        };
                        let Some((member, packet)) = ingress else {
                            return Err(TransportFault::new(
                                "control_relay",
                                role,
                                "wrong relay direction",
                            ));
                        };
                        self.mailbox
                            .push_control_inbound(member, packet)
                            .map_err(|error| TransportFault::new("control_relay", role, error))?;
                        continue;
                    }
                    let close_now = self.closing
                        && match &frame {
                            ControlFrame::Response(response) => {
                                Some(response.request_id) == self.leave_request
                            }
                            ControlFrame::Closed { .. } => true,
                            _ => false,
                        };
                    handle_frame(
                        frame,
                        &self.state,
                        &self.facts,
                        &self.control_tx,
                        &mut self.request_id,
                        &bootstrap,
                        &mut self.session,
                        &mut self.host_match,
                        &mut self.identity,
                        &mut self.members,
                        &mut self.skip_voters,
                        map_ready,
                        &mut self.last_view,
                        &mut self.started_epoch,
                        is_host,
                        role,
                    )?;
                    if close_now {
                        diag::info!(Net, "web master {role} closed: host confirmed");
                        return Ok(true);
                    }
                }
                Lane::Data => self.ingest_datagram(&bytes),
                Lane::Bulk => match decode_relay_stream(&bytes) {
                    Ok(
                        RelayDatagram::ServiceToMember(payload)
                        | RelayDatagram::HostToMember { payload, .. },
                    ) => {
                        self.bootstrap
                            .push_from_worker(None, payload.to_vec())
                            .map_err(|error| {
                                TransportFault::new("bootstrap_from_worker", role, error)
                            })?;
                    }
                    Ok(other) => {
                        diag::warn!(Net, "web master {role} ignored unexpected bulk {other:?}");
                    }
                    Err(error) => {
                        diag::warn!(Net, "web master {role} ignored bulk message: {error}");
                    }
                },
            }
        }

        // Everything queued for the wire, in the order the native writer sends it.
        while let Ok(queued) = self.control_rx.try_recv() {
            if queued.deadline <= ctx.now {
                return Err(TransportFault::new(
                    "control_write",
                    role,
                    "control queue/write deadline",
                ));
            }
            out.push((
                Lane::Control,
                encode_control(&queued.frame)
                    .map_err(|error| TransportFault::new("control_encode", role, error))?,
            ));
        }
        for (member, packet) in self.mailbox.take_outbound() {
            // Newest state wins: replace an unsent older packet for the same peer.
            if let Some((_, previous, _)) = self.pending.iter_mut().find(|(id, _, _)| *id == member)
            {
                *previous = packet;
            } else {
                if self.pending.len() >= master_protocol::MAX_SESSION_MEMBERS as usize {
                    return Err(TransportFault::new(
                        "send_datagram",
                        role,
                        "pending peer budget exhausted",
                    ));
                }
                self.pending
                    .push_back((member, packet, ctx.now + limits::values().queue_age));
            }
        }
        while let Some((member, packet, _)) = self.pending.pop_front() {
            let fragments = match self.fragmenter.split(&packet) {
                Ok(fragments) => fragments,
                Err(error) => {
                    diag::warn!(Net, "web master {role} packet not splittable: {error}");
                    continue;
                }
            };
            for fragment in &fragments {
                let datagram = if is_host {
                    RelayDatagram::HostToMember {
                        member_id: member,
                        payload: fragment,
                    }
                } else {
                    RelayDatagram::ClientToHost(fragment)
                };
                out.push((
                    Lane::Data,
                    encode_relay(datagram)
                        .map_err(|error| TransportFault::new("encode_relay", role, error.to_string()))?,
                ));
            }
        }
        while let Ok(BootstrapJob {
            match_key,
            member,
            bytes,
            deadline,
            _bytes,
        }) = self.bootstrap_rx.try_recv()
        {
            if deadline <= ctx.now {
                return Err(TransportFault::new(
                    "bootstrap_job",
                    role,
                    "bootstrap queue deadline",
                ));
            }
            let envelope = encode_relay_stream(RelayDatagram::HostToMember {
                member_id: member,
                payload: &bytes,
            })
            .map_err(|error| TransportFault::new("encode_bootstrap", role, error.to_string()))?;
            if envelope.len() > MAX_BOOTSTRAP_STREAM_BYTES {
                return Err(TransportFault::new(
                    "encode_bootstrap",
                    role,
                    format!(
                        "bootstrap envelope {} bytes exceeds the budget {MAX_BOOTSTRAP_STREAM_BYTES}",
                        envelope.len()
                    ),
                ));
            }
            out.push((Lane::Bulk, envelope));
            if let Ok(Some(BootstrapMessage::Offer {
                bootstrap_id,
                connection: conn,
                ..
            })) = decode_bootstrap(&bytes)
            {
                let _ = self.prepared_tx.try_send(HostMatchEvent::BootstrapPrepared {
                    match_key,
                    member,
                    bootstrap_id,
                    connection_id: Some(conn.0),
                });
            }
        }
        Ok(false)
    }

    fn ingest_datagram(&mut self, bytes: &[u8]) {
        let role = self.role;
        let is_host = self.is_host;
        let decoded = match decode_relay(bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                diag::warn!(Net, "web master {role} ignored relay datagram: {error}");
                return;
            }
        };
        let (member, payload) = match decoded {
            RelayDatagram::ServiceToHost { member_id, payload } if is_host => (member_id, payload),
            RelayDatagram::ServiceToMember(payload) if !is_host => (MemberId([0; 16]), payload),
            other => {
                diag::warn!(Net, "web master {role} ignored unexpected datagram {other:?}");
                return;
            }
        };
        let now = Instant::now();
        self.reassemblers
            .retain(|_, (_, seen)| now.duration_since(*seen) < limits::values().queue_age);
        if !self.reassemblers.contains_key(&member)
            && self.reassemblers.len() >= master_protocol::MAX_SESSION_MEMBERS as usize
        {
            return;
        }
        let (reassembler, seen) = self
            .reassemblers
            .entry(member)
            .or_insert_with(|| (relay_reassembler(), now));
        *seen = now;
        match reassembler.push(payload) {
            Ok(Some(packet)) => {
                if self.mailbox.push_inbound(member, packet) {
                    perf::net_leg("mail_drop_in", 0, 1);
                }
            }
            Ok(None) => {}
            Err(error) => diag::warn!(Net, "web master {role} fragment refused: {error}"),
        }
    }
}

impl Session for MatchSession {
    fn tick(&mut self, ctx: &TickCtx, inbox: &mut VecDeque<Msg>, out: &mut Vec<Msg>) -> bool {
        if self.failed {
            return true;
        }
        let outcome = self.step(ctx, inbox, out);
        let (done, error) = match outcome {
            Ok(done) => (done, None),
            Err(error) => (true, Some(error)),
        };
        if done {
            self.control_tx.close_pending();
            self.cancel.cancel();
            match error {
                Some(error) => publish_failed(&self.state, self.fallback_identity, error),
                None if self.closing => publish_left(&self.state, self.identity),
                None => {}
            }
        }
        done
    }
}

// --- the lobby list ------------------------------------------------------------

pub(super) fn spawn_browser(
    state: Arc<Mutex<MasterBrowserSnapshot>>,
    refresh: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        let local_id = hub.next_browser;
        hub.next_browser += 1;
        hub.browsers.push(Slot {
            session: Box::new(BrowseSession {
                state,
                refresh,
                cancel,
                wire: None,
                seen_refresh: 0,
                request_id: 0,
                asked_at: None,
                waiting: None,
            }),
            inbox: VecDeque::new(),
            local_id,
        });
    });
}

struct BrowseSession {
    state: Arc<Mutex<MasterBrowserSnapshot>>,
    refresh: Arc<AtomicU64>,
    cancel: CancellationToken,
    wire: Option<Wire>,
    seen_refresh: u64,
    request_id: u64,
    asked_at: Option<TickInstant>,
    waiting: Option<u64>,
}

impl Session for BrowseSession {
    fn tick(&mut self, ctx: &TickCtx, inbox: &mut VecDeque<Msg>, out: &mut Vec<Msg>) -> bool {
        if self.cancel.is_cancelled() {
            return true;
        }
        if self.wire != Some(ctx.wire) {
            self.wire = Some(ctx.wire);
            self.asked_at = None;
            self.waiting = None;
            inbox.clear();
            if let Ok(hello) = encode_control(&ControlFrame::Hello(ControlHello {
                protocol_version: master_protocol::PROTOCOL_VERSION,
                game_protocol: crate::PROTOCOL_VERSION,
                role: EndpointRole::Cli,
                build: endpoint_build(),
                player_name: String::new(),
            })) {
                out.push((Lane::Control, hello));
            }
        }
        while let Some((lane, bytes)) = inbox.pop_front() {
            if lane != Lane::Control {
                continue;
            }
            let Ok(ControlFrame::Response(response)) = decode_control(&bytes) else {
                continue;
            };
            if Some(response.request_id) != self.waiting {
                continue;
            }
            self.waiting = None;
            let mut current = self.state.lock().expect("master browser state poisoned");
            current.loading = false;
            match response.body {
                ResponseBody::RoomList {
                    generation,
                    adverts,
                } => {
                    current.generation = generation;
                    let have = current.have;
                    current.adverts = adverts
                        .into_iter()
                        .map(|advert| MasterAdvert {
                            id: advert.id,
                            name: advert.name,
                            map: advert.map,
                            mode: advert.mode,
                            players: advert.players,
                            max_players: advert.max_players,
                            locked: advert.locked,
                            in_match: advert.in_match,
                            requires: advert.requires,
                            available: advert.available,
                            password_protected: advert.password_protected,
                            missing: advert.requires.missing_from(have),
                        })
                        .collect();
                    current.error = None;
                }
                ResponseBody::Error(error) => current.error = Some(error.to_string()),
                _ => {}
            }
        }
        let refresh = self.refresh.load(Ordering::Relaxed);
        let due = self
            .asked_at
            .is_none_or(|at| ctx.now.duration_since(at) >= Duration::from_secs(2));
        if self.waiting.is_none() && (due || refresh != self.seen_refresh) {
            self.seen_refresh = refresh;
            self.request_id += 1;
            self.waiting = Some(self.request_id);
            self.asked_at = Some(ctx.now);
            self.state
                .lock()
                .expect("master browser state poisoned")
                .loading = true;
            if let Ok(request) = encode_control(&ControlFrame::Request(ControlRequest {
                request_id: self.request_id,
                body: RequestBody::ListRooms,
            })) {
                out.push((Lane::Control, request));
            }
        } else if self.waiting.is_some()
            && self
                .asked_at
                .is_some_and(|at| ctx.now.duration_since(at) >= Duration::from_secs(8))
        {
            self.waiting = None;
            let mut current = self.state.lock().expect("master browser state poisoned");
            current.loading = false;
            current.error = Some("the host did not answer: connect through the Multiplayer panel".into());
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn target() -> MasterTarget {
        MasterTarget {
            address: "web".into(),
            server_name: "web".into(),
            ca_pem: String::new(),
        }
    }

    type Wire = mpsc::Sender<(u32, u8, Vec<u8>)>;

    fn pump(sends: &Wire, member_here: u32, incoming: &mpsc::Receiver<(u32, u8, Vec<u8>)>) {
        for (_, lane, bytes) in incoming.try_iter() {
            on_message(member_here, Lane::from_u8(lane).unwrap(), bytes);
        }
        for (member, lane, bytes) in tick() {
            let _ = member;
            sends.send((member, lane as u8, bytes)).unwrap();
        }
    }

    /// A host tab and a guest tab, each with its own hub, joined by queues that
    /// stand in for the WebRTC link: the guest finds the room and joins it.
    #[test]
    fn guest_finds_and_joins_a_hosted_room() {
        let (to_guest, guest_in) = mpsc::channel();
        let (to_host, host_in) = mpsc::channel();

        let host = thread::spawn(move || {
            FORCED.with(|f| f.set(true));
            on_open(1);
            let bridge = spawn_host(
                HostConfig {
                    password: String::new(),
                    auto_start_map: false,
                    target: target(),
                    name: "friends".into(),
                    map: "mp_rust".into(),
                    mode: "war".into(),
                    max_players: 8,
                    requires: ContentFlags::IW4,
                    have: ContentFlags::IW4,
                },
                "Host".into(),
            );
            let mut hosting = false;
            for _ in 0..2000 {
                pump(&to_guest, 1, &host_in);
                if matches!(bridge.state(), MasterBridgeState::Hosting { ref members, .. } if members.len() == 2)
                {
                    hosting = true;
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            for _ in 0..50 {
                pump(&to_guest, 1, &host_in);
                thread::sleep(Duration::from_millis(2));
            }
            (hosting, format!("{:?}", bridge.state()))
        });

        let guest = thread::spawn(move || {
            FORCED.with(|f| f.set(true));
            on_open(0);
            let snapshot = Arc::new(Mutex::new(MasterBrowserSnapshot::default()));
            spawn_browser(
                Arc::clone(&snapshot),
                Arc::new(AtomicU64::new(0)),
                CancellationToken::new(),
            );
            let mut advert = None;
            for _ in 0..2000 {
                pump(&to_host, 0, &guest_in);
                if let Some(found) = snapshot.lock().unwrap().adverts.first() {
                    advert = Some(found.id);
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            let Some(advert_id) = advert else {
                return (false, "no room listed".to_owned());
            };
            let bridge = spawn_join(
                JoinConfig {
                    password: String::new(),
                    target: target(),
                    advert_id,
                    map: "mp_rust".into(),
                    mode: "war".into(),
                    have: ContentFlags::IW4,
                },
                "Guest".into(),
            );
            let mut joined = false;
            for _ in 0..2000 {
                pump(&to_host, 0, &guest_in);
                if matches!(bridge.state(), MasterBridgeState::Joined { .. }) {
                    joined = true;
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            (joined, format!("{:?}", bridge.state()))
        });

        let guest = guest.join().unwrap();
        let host = host.join().unwrap();
        assert!(guest.0, "guest state: {}", guest.1);
        assert!(host.0, "host state: {}", host.1);
    }
}
