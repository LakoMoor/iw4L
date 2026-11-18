//! The room service a hosting browser tab runs for its guests.
//!
//! Same rules as the `iw4l-master` binary (rooms, membership, content checks,
//! relay routing), minus the QUIC plumbing: peers are plain connection ids and
//! every call returns the frames to deliver instead of writing to sockets.
//! Copied from the binary so the native server stays untouched.

use std::collections::HashMap;
use std::time::Duration;

use master_protocol::{
    AdmissionFailure, Advert, AdvertId, ControlFrame, ControlRequest, ControlResponse,
    MAX_LIST_ADVERTS, MemberId, PeerEvent, RelayDatagram, RequestBody, ResponseBody, RoomPhase,
    RoomView, ServiceError, SessionCloseReason, StatusResponse, encode_relay, encode_relay_stream,
};
use web_time::Instant;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

const MAX_ROOMS: usize = 10_000;

pub(super) struct Peer {
    pub online: Option<Instant>,
    pub player_name: String,
}

struct PasswordVerifier(String);

impl PasswordVerifier {
    fn new(password: &str) -> Option<Self> {
        (!password.is_empty()).then(|| Self(password.to_owned()))
    }
    fn matches(&self, password: &str) -> bool {
        self.0.len() == password.len()
            && self
                .0
                .bytes()
                .zip(password.bytes())
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0
    }
}

pub(super) struct Room {
    view: RoomView,
    password: Option<PasswordVerifier>,
    host_connection_id: u64,
    member_of: HashMap<u64, MemberId>,
    connection_of: HashMap<MemberId, u64>,
}

#[derive(Default)]
pub(super) struct ServiceState {
    pub rooms: HashMap<AdvertId, Room>,
    pub peers: HashMap<u64, Peer>,
    pub membership: HashMap<u64, AdvertId>,
    pub generation: u64,
}

pub(super) struct HandleOutcome {
    pub response: ControlResponse,
    pub publishes: Vec<(u64, ControlFrame)>,
}

impl ServiceState {
    pub fn attach_peer(&mut self, connection_id: u64, player_name: String) {
        self.peers.insert(
            connection_id,
            Peer {
                online: None,
                player_name,
            },
        );
    }

    pub fn publish_view(&self, room_id: AdvertId) -> Vec<(u64, ControlFrame)> {
        let Some(room) = self.rooms.get(&room_id) else {
            return Vec::new();
        };
        let frame = ControlFrame::RoomView(room.view.clone());
        room.member_of
            .keys()
            .copied()
            .map(|connection_id| (connection_id, frame.clone()))
            .collect()
    }

    pub fn close_room(
        &mut self,
        room_id: AdvertId,
        reason: SessionCloseReason,
    ) -> Vec<(u64, ControlFrame)> {
        let Some(room) = self.rooms.remove(&room_id) else {
            return Vec::new();
        };
        self.generation = self.generation.wrapping_add(1);
        let frame = ControlFrame::Closed {
            room_id,
            epoch: room.view.epoch,
            reason,
        };
        let mut publishes = Vec::new();
        for connection_id in room.member_of.keys().copied() {
            self.membership.remove(&connection_id);
            publishes.push((connection_id, frame.clone()));
        }
        publishes
    }

    pub fn disconnect(&mut self, connection_id: u64) -> Vec<(u64, ControlFrame)> {
        self.peers.remove(&connection_id);
        let Some(room_id) = self.membership.get(&connection_id).copied() else {
            return Vec::new();
        };
        let Some(room) = self.rooms.get(&room_id) else {
            self.membership.remove(&connection_id);
            return Vec::new();
        };
        if room.host_connection_id == connection_id {
            return self.close_room(room_id, SessionCloseReason::HostLeft);
        }
        self.remove_member(connection_id)
    }

    pub fn remove_member(&mut self, connection_id: u64) -> Vec<(u64, ControlFrame)> {
        let Some(room_id) = self.membership.remove(&connection_id) else {
            return Vec::new();
        };
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return Vec::new();
        };
        let Some(member_id) = room.member_of.remove(&connection_id) else {
            return Vec::new();
        };
        room.connection_of.remove(&member_id);
        room.view.members.retain(|id| *id != member_id);
        room.view.member_names.remove(&member_id);
        room.view.revision = room.view.revision.wrapping_add(1).max(1);
        self.generation = self.generation.wrapping_add(1);
        self.publish_view(room_id)
    }

    pub fn bump_room(&mut self, room_id: AdvertId) {
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.view.revision = room.view.revision.wrapping_add(1).max(1);
        }
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn host_of(&self, connection_id: u64) -> std::result::Result<AdvertId, ServiceError> {
        let room_id = *self
            .membership
            .get(&connection_id)
            .ok_or(ServiceError::NotMember)?;
        let room = self
            .rooms
            .get(&room_id)
            .ok_or(ServiceError::UnknownAdvert)?;
        if room.host_connection_id != connection_id {
            return Err(ServiceError::NotHost);
        }
        Ok(room_id)
    }

    pub fn member_of(
        &self,
        connection_id: u64,
    ) -> std::result::Result<(AdvertId, MemberId), ServiceError> {
        let room_id = *self
            .membership
            .get(&connection_id)
            .ok_or(ServiceError::NotMember)?;
        let room = self
            .rooms
            .get(&room_id)
            .ok_or(ServiceError::UnknownAdvert)?;
        let member_id = *room
            .member_of
            .get(&connection_id)
            .ok_or(ServiceError::NotMember)?;
        Ok((room_id, member_id))
    }

    fn route(
        &self,
        connection_id: u64,
        relay: &RelayDatagram<'_>,
        stream: bool,
    ) -> Result<(u64, Vec<u8>)> {
        let encode = |datagram: RelayDatagram<'_>| {
            if stream {
                encode_relay_stream(datagram)
            } else {
                encode_relay(datagram)
            }
        };
        match relay {
            RelayDatagram::ClientToHost(payload) => {
                let (room_id, member_id) = self.member_of(connection_id)?;
                let room = self.rooms.get(&room_id).ok_or(ServiceError::UnknownAdvert)?;
                if !self.peers.contains_key(&room.host_connection_id) {
                    return Err(ServiceError::NotHost.into());
                }
                let outgoing = encode(RelayDatagram::ServiceToHost { member_id, payload })?;
                Ok((room.host_connection_id, outgoing))
            }
            RelayDatagram::HostToMember { member_id, payload } => {
                let room_id = self.host_of(connection_id)?;
                let room = self.rooms.get(&room_id).ok_or(ServiceError::UnknownAdvert)?;
                let target = *room
                    .connection_of
                    .get(member_id)
                    .ok_or(ServiceError::NotMember)?;
                if !self.peers.contains_key(&target) {
                    return Err(ServiceError::NotMember.into());
                }
                let outgoing = encode(RelayDatagram::ServiceToMember(payload))?;
                Ok((target, outgoing))
            }
            RelayDatagram::ServiceToHost { .. } | RelayDatagram::ServiceToMember(_) => {
                Err(ServiceError::Malformed.into())
            }
        }
    }

    /// Where a gameplay datagram goes, and the bytes to deliver there.
    pub fn route_datagram(
        &self,
        connection_id: u64,
        relay: &RelayDatagram<'_>,
    ) -> Result<(u64, Vec<u8>)> {
        self.route(connection_id, relay, false)
    }

    /// Where a control-lane or bootstrap relay goes, and the bytes to deliver there.
    pub fn route_stream(
        &self,
        connection_id: u64,
        relay: &RelayDatagram<'_>,
    ) -> Result<(u64, Vec<u8>)> {
        self.route(connection_id, relay, true)
    }

    /// Answer one request: the response for the sender, plus frames for others.
    pub fn request(&mut self, connection_id: u64, request: ControlRequest) -> HandleOutcome {
        handle_request(self, connection_id, request)
    }
}

fn handle_request(
    state: &mut ServiceState,
    connection_id: u64,
    request: ControlRequest,
) -> HandleOutcome {
    let request_id = request.request_id;
    let (body, publishes) = match request.body {
        RequestBody::Status => (ResponseBody::Status(StatusResponse::CURRENT), Vec::new()),
        RequestBody::Population { online } => {
            if let Some(peer) = state.peers.get_mut(&connection_id) {
                peer.online = online.then(Instant::now);
            }
            let players = state.membership.len()
                + state
                    .peers
                    .iter()
                    .filter(|(id, peer)| {
                        peer.online
                            .is_some_and(|seen| seen.elapsed() < Duration::from_secs(10))
                            && !state.membership.contains_key(id)
                    })
                    .count();
            (
                ResponseBody::Population {
                    players: u32::try_from(players).unwrap_or(u32::MAX),
                },
                Vec::new(),
            )
        }
        RequestBody::CreateRoom {
            name,
            map,
            mode,
            max_players,
            requires,
            available,
            password,
        } => create_room(
            state,
            connection_id,
            name,
            map,
            mode,
            max_players,
            requires,
            available,
            password,
        ),
        RequestBody::JoinRoom {
            room_id,
            have,
            password,
        } => join_room(state, connection_id, room_id, have, password),
        RequestBody::SetPassword { password } => match state.host_of(connection_id) {
            Ok(room_id) => {
                let room = state.rooms.get_mut(&room_id).expect("host room");
                room.password = PasswordVerifier::new(&password);
                room.view.password_protected = room.password.is_some();
                state.bump_room(room_id);
                (
                    ResponseBody::RoomUpdated {
                        view: state.rooms[&room_id].view.clone(),
                    },
                    state.publish_view(room_id),
                )
            }
            Err(error) => (ResponseBody::Error(error), Vec::new()),
        },
        RequestBody::LeaveRoom => leave_room(state, connection_id),
        RequestBody::SetOptions {
            map,
            mode,
            joinable,
        } => set_options(state, connection_id, map, mode, joinable),
        RequestBody::StartMatch { map, mode } => start_match(state, connection_id, map, mode),
        RequestBody::EndMatch { room_id, epoch } => {
            end_match(state, connection_id, room_id, epoch)
        }
        RequestBody::CloseRoom => match state.host_of(connection_id) {
            Ok(room_id) => (
                ResponseBody::Ack,
                state.close_room(room_id, SessionCloseReason::HostLeft),
            ),
            Err(error) => (ResponseBody::Error(error), Vec::new()),
        },
        RequestBody::ListRooms => {
            let mut adverts: Vec<Advert> = state
                .rooms
                .values()
                .map(|room| room.view.advert())
                .collect();
            adverts.sort_by_key(|advert| advert.id);
            adverts.truncate(MAX_LIST_ADVERTS);
            (
                ResponseBody::RoomList {
                    generation: state.generation,
                    adverts,
                },
                Vec::new(),
            )
        }
        RequestBody::HostWorldReady {
            room_id,
            epoch,
            map,
            weapons,
            classes,
        } => host_world_ready(
            state,
            connection_id,
            room_id,
            epoch,
            map,
            weapons,
            classes,
        ),
        RequestBody::MapLoaded {
            epoch,
            map,
            weapons,
            classes,
        } => forward_to_host(state, connection_id, |member_id| PeerEvent::MapLoaded {
            member_id,
            epoch,
            map,
            weapons,
            classes,
        }),
        RequestBody::BootstrapApplied {
            epoch,
            bootstrap_id,
            snapshot_seq,
            connection,
        } => forward_to_host(state, connection_id, |member_id| {
            PeerEvent::BootstrapApplied {
                member_id,
                epoch,
                bootstrap_id,
                snapshot_seq,
                connection,
            }
        }),
        RequestBody::EnterMatch {
            member_id,
            epoch,
            bootstrap_id,
            connection_id: admitted_connection,
            client_id,
        } => enter_match(
            state,
            connection_id,
            member_id,
            epoch,
            bootstrap_id,
            admitted_connection,
            client_id,
        ),
        RequestBody::AdmissionFailed {
            member_id,
            epoch,
            reason,
        } => admission_failed(state, connection_id, member_id, epoch, reason),
        RequestBody::VoteToSkip => forward_to_host(state, connection_id, |member_id| {
            PeerEvent::VoteToSkip { member_id }
        }),
    };
    HandleOutcome {
        response: ControlResponse { request_id, body },
        publishes,
    }
}

fn create_room(
    state: &mut ServiceState,
    connection_id: u64,
    name: String,
    map: String,
    mode: String,
    max_players: u8,
    requires: master_protocol::ContentFlags,
    available: master_protocol::ContentFlags,
    password: String,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    if !master_protocol::ContentFlags::KNOWN.contains(requires)
        || !requires.contains(master_protocol::ContentFlags::IW4)
        || !requires.missing_from(available).is_empty()
        || !map_content_allowed(&map, requires)
    {
        return (ResponseBody::Error(ServiceError::InvalidAdvert), Vec::new());
    }
    if state.membership.contains_key(&connection_id) {
        return (
            ResponseBody::Error(ServiceError::AlreadyHosting),
            Vec::new(),
        );
    }
    if state.rooms.len() >= MAX_ROOMS {
        return (
            ResponseBody::Error(ServiceError::AdvertCapacity),
            Vec::new(),
        );
    }
    let room_id = AdvertId(random_bytes());
    let member_id = MemberId(random_bytes());
    let view = RoomView {
        room_id,
        name,
        host: member_id,
        members: vec![member_id],
        member_names: [(member_id, state.peers[&connection_id].player_name.clone())].into(),
        map,
        mode,
        joinable: true,
        revision: 1,
        epoch: 0,
        phase: RoomPhase::Gathering,
        max_players,
        requires,
        available,
        password_protected: !password.is_empty(),
    };
    let mut member_of = HashMap::new();
    member_of.insert(connection_id, member_id);
    let mut connection_of = HashMap::new();
    connection_of.insert(member_id, connection_id);
    state.rooms.insert(
        room_id,
        Room {
            view: view.clone(),
            password: PasswordVerifier::new(&password),
            host_connection_id: connection_id,
            member_of,
            connection_of,
        },
    );
    state.membership.insert(connection_id, room_id);
    state.generation = state.generation.wrapping_add(1);
    (
        ResponseBody::RoomCreated {
            member_id,
            view: view.clone(),
        },
        vec![(connection_id, ControlFrame::RoomView(view))],
    )
}

fn join_room(
    state: &mut ServiceState,
    connection_id: u64,
    room_id: AdvertId,
    have: master_protocol::ContentFlags,
    password: String,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    if let Some(existing) = state.membership.get(&connection_id).copied() {
        if existing == room_id {
            let room = state.rooms.get(&room_id).expect("membership names a room");
            let member_id = *room.member_of.get(&connection_id).expect("host/member map");
            return (
                ResponseBody::RoomJoined {
                    member_id,
                    view: room.view.clone(),
                },
                Vec::new(),
            );
        }
        return (
            ResponseBody::Error(ServiceError::AlreadyHosting),
            Vec::new(),
        );
    }
    let Some(room) = state.rooms.get(&room_id) else {
        return (ResponseBody::Error(ServiceError::UnknownAdvert), Vec::new());
    };
    if room
        .password
        .as_ref()
        .is_some_and(|verifier| !verifier.matches(&password))
    {
        return (
            ResponseBody::Error(ServiceError::IncorrectPassword),
            Vec::new(),
        );
    }
    if !room.view.advert().allows_join_request() {
        return (ResponseBody::Error(ServiceError::Locked), Vec::new());
    }
    if !room.view.requires.missing_from(have).is_empty() {
        return (
            ResponseBody::Error(ServiceError::MissingContent {
                requires: room.view.requires,
                have,
            }),
            Vec::new(),
        );
    }
    if room.view.members.len() >= room.view.max_players as usize {
        return (ResponseBody::Error(ServiceError::Full), Vec::new());
    }
    let member_id = MemberId(random_bytes());
    let room = state.rooms.get_mut(&room_id).expect("room still present");
    room.member_of.insert(connection_id, member_id);
    room.connection_of.insert(member_id, connection_id);
    room.view.members.push(member_id);
    room.view
        .member_names
        .insert(member_id, state.peers[&connection_id].player_name.clone());
    room.view.revision = room.view.revision.wrapping_add(1).max(1);
    state.membership.insert(connection_id, room_id);
    state.generation = state.generation.wrapping_add(1);
    let view = room.view.clone();
    (
        ResponseBody::RoomJoined {
            member_id,
            view: view.clone(),
        },
        state.publish_view(room_id),
    )
}

fn leave_room(
    state: &mut ServiceState,
    connection_id: u64,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    let Some(room_id) = state.membership.get(&connection_id).copied() else {
        return (ResponseBody::RoomLeft, Vec::new());
    };
    let publishes = if state
        .rooms
        .get(&room_id)
        .is_some_and(|room| room.host_connection_id == connection_id)
    {
        state.close_room(room_id, SessionCloseReason::HostLeft)
    } else {
        state.remove_member(connection_id)
    };
    (ResponseBody::RoomLeft, publishes)
}

fn map_content_allowed(map: &str, families: master_protocol::ContentFlags) -> bool {
    master_protocol::ContentFlags::for_map(map).is_some_and(|family| families.contains(family))
}

fn set_options(
    state: &mut ServiceState,
    connection_id: u64,
    map: String,
    mode: String,
    joinable: bool,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.host_of(connection_id) {
        Ok(room_id) => {
            let room = state.rooms.get_mut(&room_id).expect("host room");
            if !map_content_allowed(&map, room.view.requires)
                || (room.view.phase.in_match() && (room.view.map != map || room.view.mode != mode))
            {
                return (ResponseBody::Error(ServiceError::InvalidAdvert), Vec::new());
            }
            room.view.map = map;
            room.view.mode = mode;
            room.view.joinable = joinable;
            state.bump_room(room_id);
            let view = state.rooms[&room_id].view.clone();
            (
                ResponseBody::RoomUpdated { view: view.clone() },
                state.publish_view(room_id),
            )
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

fn start_match(
    state: &mut ServiceState,
    connection_id: u64,
    map: String,
    mode: String,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.host_of(connection_id) {
        Ok(room_id) => {
            let room = state.rooms.get_mut(&room_id).expect("host room");
            if !map_content_allowed(&map, room.view.requires)
                || (room.view.phase.in_match() && (room.view.map != map || room.view.mode != mode))
            {
                return (ResponseBody::Error(ServiceError::InvalidAdvert), Vec::new());
            }
            room.view.map = map;
            room.view.mode = mode;
            if room.view.phase.in_match() {
                return (
                    ResponseBody::RoomUpdated {
                        view: room.view.clone(),
                    },
                    Vec::new(),
                );
            } else {
                room.view.epoch = room.view.epoch.wrapping_add(1).max(1);
                room.view.phase = RoomPhase::Loading;
            }
            state.bump_room(room_id);
            let view = state.rooms[&room_id].view.clone();
            (
                ResponseBody::RoomUpdated { view: view.clone() },
                state.publish_view(room_id),
            )
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

fn end_match(
    state: &mut ServiceState,
    connection_id: u64,
    expected_room: AdvertId,
    epoch: u32,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.host_of(connection_id) {
        Ok(room_id) => {
            let room = state.rooms.get_mut(&room_id).expect("host room");
            if room_id != expected_room
                || epoch == 0
                || room.view.epoch != epoch
                || !room.view.phase.in_match()
            {
                return (ResponseBody::Ack, Vec::new());
            }
            room.view.phase = RoomPhase::Gathering;
            state.bump_room(room_id);
            (ResponseBody::Ack, state.publish_view(room_id))
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

fn host_world_ready(
    state: &mut ServiceState,
    connection_id: u64,
    expected_room: AdvertId,
    epoch: u32,
    map: u64,
    weapons: u64,
    classes: u64,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.host_of(connection_id) {
        Ok(room_id) => {
            let room = state.rooms.get_mut(&room_id).expect("host room");
            if room_id != expected_room
                || epoch == 0
                || room.view.epoch != epoch
                || !room.view.phase.in_match()
            {
                return (ResponseBody::Ack, Vec::new());
            }
            room.view.phase = RoomPhase::Running;
            state.bump_room(room_id);
            let mut publishes = state.publish_view(room_id);
            let event = ControlFrame::PeerEvent(PeerEvent::HostWorldReady {
                epoch,
                map,
                weapons,
                classes,
            });
            if let Some(room) = state.rooms.get(&room_id) {
                for peer in room.member_of.keys().copied() {
                    publishes.push((peer, event.clone()));
                }
            }
            (ResponseBody::Ack, publishes)
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

fn forward_to_host(
    state: &ServiceState,
    connection_id: u64,
    event: impl FnOnce(MemberId) -> PeerEvent,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.member_of(connection_id) {
        Ok((room_id, member_id)) => {
            let Some(room) = state.rooms.get(&room_id) else {
                return (ResponseBody::Error(ServiceError::UnknownAdvert), Vec::new());
            };
            (
                ResponseBody::Ack,
                vec![(
                    room.host_connection_id,
                    ControlFrame::PeerEvent(event(member_id)),
                )],
            )
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

fn enter_match(
    state: &ServiceState,
    connection_id: u64,
    member_id: MemberId,
    epoch: u32,
    bootstrap_id: u32,
    admitted_connection: u64,
    client_id: u32,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.host_of(connection_id) {
        Ok(room_id) => {
            let Some(room) = state.rooms.get(&room_id) else {
                return (ResponseBody::Error(ServiceError::UnknownAdvert), Vec::new());
            };
            let Some(&target) = room.connection_of.get(&member_id) else {
                return (ResponseBody::Error(ServiceError::NotMember), Vec::new());
            };
            (
                ResponseBody::Ack,
                vec![(
                    target,
                    ControlFrame::PeerEvent(PeerEvent::EnterMatch {
                        member_id,
                        epoch,
                        bootstrap_id,
                        connection_id: admitted_connection,
                        client_id,
                    }),
                )],
            )
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

fn admission_failed(
    state: &ServiceState,
    connection_id: u64,
    member_id: MemberId,
    epoch: u32,
    reason: AdmissionFailure,
) -> (ResponseBody, Vec<(u64, ControlFrame)>) {
    match state.host_of(connection_id) {
        Ok(room_id) => {
            let Some(room) = state.rooms.get(&room_id) else {
                return (ResponseBody::Error(ServiceError::UnknownAdvert), Vec::new());
            };
            let Some(&target) = room.connection_of.get(&member_id) else {
                return (ResponseBody::Error(ServiceError::NotMember), Vec::new());
            };
            (
                ResponseBody::Ack,
                vec![(
                    target,
                    ControlFrame::PeerEvent(PeerEvent::AdmissionFailed {
                        member_id,
                        epoch,
                        reason,
                    }),
                )],
            )
        }
        Err(error) => (ResponseBody::Error(error), Vec::new()),
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("randomness unavailable");
    bytes
}

pub fn normalize_player_name(name: &str) -> String {
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(16)
        .collect();
    if name.is_empty() {
        "Player".into()
    } else {
        name
    }
}
