use std::{collections::BTreeSet, ops::Sub};

use minicbor::{Decode, Encode};
use rand::Rng;
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

use crate::network::{IncomingMessage, NodeId};

#[derive(Decode, Encode, Debug, Clone, PartialEq, Eq)]
pub enum RaftMessage {
    #[n(0)]
    Connect,
    #[n(1)]
    Disconnect,
    #[n(2)]
    RequestVote {
        #[n(0)]
        term: usize,
    },
    #[n(3)]
    RequestVoteResponse {
        #[n(0)]
        term: usize,
        #[n(1)]
        vote: bool,
    },
    #[n(4)]
    Heartbeat {
        #[n(0)]
        term: usize,
    },
}

#[derive(Eq, PartialEq, Clone)]
pub enum RaftStatus {
    Follower {
        leader: Option<NodeId>,
        voted_for: Option<NodeId>,
    },
    Candidate {
        votes: BTreeSet<NodeId>,
    },
    Leader {
        abdicating: bool,
    },
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub enum RaftLeader {
    Myself,
    Other(NodeId),
    Unknown,
}

pub struct RaftState {
    pub id: NodeId,
    pub quorum: usize,
    pub peers: BTreeSet<NodeId>,
    warned_about_quorum: bool,

    pub last_event: Instant,
    pub heartbeat_freq: Duration,
    pub timeout_freq: Duration,
    pub jitter: Duration,

    pub status: RaftStatus,
    pub term: usize,

    pub leader_sink: watch::Sender<RaftLeader>,
}

impl RaftState {
    pub fn new(
        id: NodeId,
        start_time: Instant,
        quorum: usize,
        heartbeat_freq: Duration,
        timeout_freq: Duration,
        leader_sink: watch::Sender<RaftLeader>,
    ) -> Self {
        let mut state = RaftState {
            id,
            quorum,
            warned_about_quorum: false,
            peers: BTreeSet::new(),
            last_event: start_time,
            heartbeat_freq,
            timeout_freq,
            jitter: Duration::from_millis(rand::thread_rng().gen_range(50..100)),
            status: RaftStatus::Follower {
                leader: None,
                voted_for: None,
            },
            term: 0,
            leader_sink,
        };
        state.clear_status();
        state
    }

    // How long until the next time we need to take action on our own?
    pub fn next_event(&self, now: Instant) -> Instant {
        // The default wait time is 10 seconds.
        // This is only relevant when there aren't enough nodes connected to do raft,
        // and it just controls how long we wait to warn about not having quorum
        let mut next_event = now + Duration::from_secs(10);

        let can_reach_quorum = (self.peers.len() + 1) >= self.quorum;
        if can_reach_quorum {
            // If we can reach quorum, this is when we'll hold our next election.
            let election_time = self.last_event + self.timeout_freq.sub(self.jitter);
            next_event = next_event.min(election_time);
        }

        if self.is_leader() {
            // If we're the leader, this is when we'll send our next heartbeat.
            let heartbeat_time = self.last_event + self.heartbeat_freq;
            next_event = next_event.min(heartbeat_time);
        }

        next_event.max(now)
    }

    fn leader(&self) -> Option<NodeId> {
        match &self.status {
            RaftStatus::Leader { .. } => Some(self.id.clone()),
            RaftStatus::Follower {
                leader: Some(leader),
                ..
            } => Some(leader.clone()),
            _ => None,
        }
    }

    fn is_leader(&self) -> bool {
        matches!(self.status, RaftStatus::Leader { .. })
    }

    pub fn abdicate(&mut self) -> Vec<(NodeId, RaftMessage)> {
        if let RaftStatus::Leader { abdicating } = &mut self.status {
            *abdicating = true;
        }
        vec![]
    }

    pub fn receive(
        &mut self,
        timestamp: Instant,
        message: IncomingMessage<RaftMessage>,
    ) -> Vec<(NodeId, RaftMessage)> {
        let from = message.from;
        match &message.data {
            RaftMessage::Connect => {
                info!("New peer {}", from);
                self.peers.insert(from.clone());
                if matches!(self.status, RaftStatus::Leader { abdicating: false }) {
                    // Let them know right away that we're the leader
                    let response = RaftMessage::Heartbeat { term: self.term };
                    vec![(from.clone(), response)]
                } else {
                    vec![]
                }
            }
            RaftMessage::Disconnect => {
                info!("Peer disconnected {}", from);
                self.peers.remove(&from);
                if self.peers.len() < self.quorum - 1 {
                    info!("Too few peers connected, raft status unknown");
                    self.clear_status();
                } else if self.leader() == Some(from) {
                    info!("Current leader has disconnected");
                    self.clear_status();
                }
                vec![]
            }
            RaftMessage::Heartbeat { term } => {
                let current_leader = self.leader();
                if *term >= self.term {
                    self.term = *term;
                    self.set_status(RaftStatus::Follower {
                        leader: Some(from.clone()),
                        voted_for: None,
                    });
                    if Some(from.clone()) != current_leader {
                        info!(term = term, leader = %from, "New leader");
                    }
                    self.warned_about_quorum = false;
                }
                self.last_event = timestamp;
                vec![]
            }
            RaftMessage::RequestVote {
                term: requested_term,
            } => {
                debug!(term = requested_term, "Vote requested by {}", from);
                let peer = from.clone();
                let is_new_term = *requested_term > self.term;
                let (vote, reason) = if is_new_term {
                    self.term = *requested_term;
                    (true, "First candidate of new term")
                } else {
                    match &self.status {
                        RaftStatus::Leader { .. } => (false, "Already elected self"),
                        RaftStatus::Follower {
                            leader: _,
                            voted_for: Some(candidate),
                        } => {
                            if candidate == &from {
                                (true, "Already voted for this candidate")
                            } else {
                                (false, "Already voted for another candidate")
                            }
                        }
                        RaftStatus::Follower {
                            leader: _,
                            voted_for: None,
                        } => (true, "First candidate of term"),
                        RaftStatus::Candidate { .. } => (false, "Already voted for self"),
                    }
                };

                debug!(
                    term = requested_term,
                    reason = reason,
                    vote = vote,
                    "Casting vote for {}'s election",
                    peer
                );
                if vote {
                    let mut leader = None;
                    let mut old_voted_for = None;
                    if let RaftStatus::Follower {
                        leader: current_leader,
                        voted_for,
                    } = &self.status
                    {
                        old_voted_for.clone_from(voted_for);
                        if !is_new_term {
                            leader.clone_from(current_leader);
                        }
                    };
                    if !old_voted_for.is_some_and(|id| id == from) {
                        // If we're newly voting for someone, reset the election timeout
                        self.last_event = timestamp;
                    }
                    self.set_status(RaftStatus::Follower {
                        leader,
                        voted_for: Some(from.clone()),
                    });
                }
                let response = RaftMessage::RequestVoteResponse {
                    term: self.term,
                    vote,
                };
                vec![(peer, response)]
            }
            RaftMessage::RequestVoteResponse { term, vote, .. } => {
                match &self.status {
                    RaftStatus::Candidate { votes: prev_votes } => {
                        if *term < self.term {
                            return vec![];
                        }
                        if *term > self.term {
                            debug!(
                                term,
                                my_term = self.term,
                                "Updating to match peer's newer term"
                            );
                            self.term = *term;
                        }
                        if !vote {
                            debug!(
                                votes = prev_votes.len(),
                                term,
                                quorum = self.quorum,
                                "Vote rejected"
                            );
                            return vec![];
                        }

                        let mut new_votes = prev_votes.clone();
                        new_votes.insert(from.clone());
                        debug!(
                            votes = new_votes.len(),
                            term,
                            quorum = self.quorum,
                            "Vote received"
                        );
                        self.set_status(RaftStatus::Candidate {
                            votes: new_votes.clone(),
                        });
                        if new_votes.len() >= self.quorum {
                            info!(
                                term = term,
                                votes = new_votes.len(),
                                quorum = self.quorum,
                                "Election won"
                            );
                            self.set_status(RaftStatus::Leader { abdicating: false });
                            // Immediately send a heartbeat to make leader election stable
                            return self
                                .peers
                                .iter()
                                .map(|peer| (peer.clone(), RaftMessage::Heartbeat { term: *term }))
                                .collect();
                        } else {
                            vec![]
                        }
                    }
                    _ => {
                        if *term >= self.term {
                            warn!(term = term, from = %from, "Unexpected message {:?}", message.data);
                        }
                        vec![]
                    }
                }
            }
        }
    }

    pub fn tick(&mut self, timestamp: Instant) -> Vec<(NodeId, RaftMessage)> {
        let actual_timeout = self.timeout_freq.sub(self.jitter);
        let elapsed_time = timestamp.duration_since(self.last_event);
        let heartbeat_timeout = elapsed_time > self.heartbeat_freq;
        let election_timeout = elapsed_time > actual_timeout;
        let can_reach_quorum = (self.peers.len() + 1) >= self.quorum;
        let (is_leader, abdicating) = if let RaftStatus::Leader { abdicating } = &self.status {
            (true, *abdicating)
        } else {
            (false, false)
        };

        if election_timeout && !can_reach_quorum {
            if !self.warned_about_quorum {
                warn!(
                    term = self.term,
                    nodes = self.peers.len() + 1,
                    quorum = self.quorum,
                    "Timeout (with leader) reached after {:?}, but can't reach quorum",
                    actual_timeout
                );
                self.warned_about_quorum = true;
            }
            vec![]
        } else if election_timeout && can_reach_quorum && !abdicating {
            info!(
                term = self.term + 1,
                nodes = self.peers.len() + 1,
                quorum = self.quorum,
                ?self.last_event,
                "Timeout reached after {:?}, starting a new election",
                actual_timeout
            );
            // Update jitter to avoid aggressive election cycles
            self.jitter = Duration::from_millis((rand::random::<u8>() % 50 + 50) as u64);

            // Start an election
            let mut votes = BTreeSet::new();
            votes.insert(self.id.clone());
            self.set_status(RaftStatus::Candidate { votes });
            self.term += 1;
            self.last_event = timestamp;

            return self
                .peers
                .iter()
                .map(|peer| (peer.clone(), RaftMessage::RequestVote { term: self.term }))
                .collect();
        } else if is_leader && heartbeat_timeout && !abdicating {
            self.emit_has_leader(true, true);
            if !self.peers.is_empty() {
                debug!("Sending heartbeats as leader");
                // Send heartbeats
                self.last_event = timestamp;
                self.peers
                    .iter()
                    .map(|peer| (peer.clone(), RaftMessage::Heartbeat { term: self.term }))
                    .collect()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    }

    fn clear_status(&mut self) {
        self.set_status(RaftStatus::Follower {
            leader: None,
            voted_for: None,
        });
    }

    fn set_status(&mut self, status: RaftStatus) {
        let leader = match &status {
            RaftStatus::Leader { .. } => RaftLeader::Myself,
            RaftStatus::Follower {
                leader: Some(leader),
                ..
            } => RaftLeader::Other(leader.clone()),
            _ => RaftLeader::Unknown,
        };
        self.emit_has_leader(leader == RaftLeader::Myself, leader != RaftLeader::Unknown);
        self.status = status;
        self.leader_sink.send_if_modified(|old_leader| {
            let changed = *old_leader != leader;
            if changed {
                *old_leader = leader;
            }
            changed
        });
    }

    fn emit_has_leader(&self, is_leader: bool, has_leader: bool) {
        let has_leader: u64 = if has_leader { 1 } else { 0 };
        let is_leader: u64 = if is_leader { 1 } else { 0 };
        debug!(
            histogram.has_leader = has_leader,
            histogram.is_leader = is_leader,
            "leader metrics",
        );
    }
}
