use std::{collections::HashSet, ops::Sub};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{info, trace, warn};


#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RaftMessage {
    Connect { node_id: String },
    Disconnect { node_id: String },
    RequestVote { node_id: String, term: usize },
    RequestVoteResponse { term: usize, vote: bool },
    Heartbeat { leader: String, term: usize },
}

#[derive(Eq, PartialEq)]
pub enum RaftStatus {
    Follower { leader: Option<String>, voted_for: Option<String> },
    Candidate { votes: usize },
    Leader,
}
pub struct RaftState {
    pub id: String,
    pub quorum: usize,
    pub peers: HashSet<String>,

    pub last_heartbeat: std::time::Instant,
    pub heartbeat_freq: std::time::Duration,
    pub timeout_freq: std::time::Duration,
    pub jitter: std::time::Duration,

    pub status: RaftStatus,
    pub term: usize,
}

impl RaftState {
    pub fn new(
        id: String,
        quorum: usize,
        heartbeat_freq: std::time::Duration,
        timeout_freq: std::time::Duration,
    ) -> Self {
        RaftState {
            id,
            quorum: quorum,
            peers: HashSet::new(),
            last_heartbeat: std::time::Instant::now(),
            heartbeat_freq,
            timeout_freq,
            jitter: std::time::Duration::from_millis(rand::thread_rng().gen_range(50..100)),
            status: RaftStatus::Follower { leader: None, voted_for: None },
            term: 0,
        }
    }

    pub fn leader(&self) -> Option<String> {
        match &self.status {
            RaftStatus::Leader => Some(self.id.clone()),
            RaftStatus::Follower { leader: Some(leader), .. } => Some(leader.clone()),
            _ => None,
        }
    }

    pub fn is_leader(&self) -> bool {
        matches!(self.status, RaftStatus::Leader)
    }

    pub fn receive(&mut self, timestamp: std::time::Instant, message: RaftMessage) -> Vec<(String, RaftMessage)> {
        match message {
            RaftMessage::Connect { node_id } => {
                info!(me = self.id, "New peer {}", node_id);
                self.peers.insert(node_id.clone());
                if matches!(self.status, RaftStatus::Leader) {
                    // Let them know right away that we're the leader
                    let response = RaftMessage::Heartbeat { leader: self.id.clone(), term: self.term };
                    vec![(node_id, response)]
                } else {
                    vec![]
                }
            },
            RaftMessage::Disconnect { node_id } => {
                info!(me = self.id, "Peer disconnected {}", node_id);
                self.peers.remove(&node_id);
                vec![]
            },
            RaftMessage::Heartbeat { leader, term } => {
                let current_leader = self.leader();
                if term >= self.term {
                    self.term = term;
                    self.status = RaftStatus::Follower { leader: Some(leader.clone()), voted_for: None };
                    if Some(leader.clone()) != current_leader {
                        info!(me = self.id, term = term, leader = leader, "New leader");
                    }
                }
                self.last_heartbeat = timestamp;
                vec![]
            },
            RaftMessage::RequestVote { node_id, term: requested_term } => {
                trace!(me = self.id, term = requested_term, "Vote requested by {}", node_id);
                let peer = node_id.clone();
                let (vote, reason) = match self.status {
                    RaftStatus::Follower { leader: _, voted_for: Some(_) } => {
                        (false, "Already voted")
                    }
                    RaftStatus::Leader | RaftStatus::Follower { leader: _, voted_for: None } => {
                        if requested_term > self.term {
                            // Vote for the candidate
                            let current_leader = match &self.status {
                                RaftStatus::Follower { leader, voted_for: _ } => leader.clone(),
                                RaftStatus::Leader => Some(self.id.clone()),
                                RaftStatus::Candidate { .. } => None,
                            };
                            self.status = RaftStatus::Follower { leader: current_leader, voted_for: Some(node_id) };
                            (true, "newer term with note vote")
                        } else {
                            (false, "old term")
                        }
                    }
                    RaftStatus::Candidate { .. } => {
                        if requested_term > self.term {
                            self.status = RaftStatus::Follower { leader: None, voted_for: Some(node_id) };
                            (true, "newer term with vote")
                        } else {
                            (false, "already voted for self")
                        }
                    }
                };
                trace!(me = self.id, term = requested_term, reason=reason, vote=vote, "Casting vote for {}'s election", peer);
                let response = RaftMessage::RequestVoteResponse { term: requested_term, vote };
                return vec![(peer, response)];
            },
            RaftMessage::RequestVoteResponse { term, vote } => {
                match self.status {
                    RaftStatus::Candidate { votes: prev_votes } => {
                        if term != self.term {
                            return vec![];
                        }
                        if !vote {
                            trace!(me = self.id, votes = prev_votes, term = term, my_term = self.term, quorum = self.quorum, "Vote rejected");
                            return vec![];
                        }

                        let new_votes = prev_votes + 1;
                        trace!(me = self.id, votes = new_votes, quorum = self.quorum, "Vote received");
                        self.status = RaftStatus::Candidate { votes: new_votes };
                        if new_votes >= self.quorum {
                            info!(me = self.id, term = term, votes = new_votes, quorum = self.quorum, "Election won");
                            self.status = RaftStatus::Leader;
                            // Immediately send a heartbeat to make leader election stable
                            return self.peers.iter().map(|peer| {
                                (peer.clone(), RaftMessage::Heartbeat { leader: self.id.clone(), term })
                            }).collect();
                        } else {
                            vec![]
                        }
                    },
                    _ => {
                        warn!(me = self.id, term = term, "Unexpected message");
                        vec![]
                    }
                }
            }
        }
    }

    pub fn tick(&mut self, timestamp: std::time::Instant) -> Vec<(String, RaftMessage)> {
        let actual_timeout = self.timeout_freq.sub(self.jitter);
        if self.last_heartbeat.elapsed() > actual_timeout && self.peers.len() + 1 >= self.quorum {
            info!(me = self.id, term = self.term + 1, nodes = self.peers.len() + 1, quorum = self.quorum, "Timeout reached after {:?}, starting a new election", actual_timeout);
            // Update jitter to avoid aggressive election cycles
            self.jitter = std::time::Duration::from_millis((rand::random::<u8>() % 50 + 50) as u64);

            // Start an election
            self.status = RaftStatus::Candidate { votes: 1 };
            self.term += 1;
            self.last_heartbeat = timestamp;

            let message = RaftMessage::RequestVote { node_id: self.id.clone(), term: self.term };
            return self.peers.iter().map(|peer| {
                (peer.clone(), message.clone())
            }).collect();
        } else if self.status == RaftStatus::Leader && self.last_heartbeat.elapsed() > self.heartbeat_freq {
            trace!(me = self.id, "Sending heartbeats as leader");
            // Send heartbeats
            self.last_heartbeat = timestamp;
            let message = RaftMessage::Heartbeat { leader: self.id.clone(), term: self.term };
            return self.peers.iter().map(|peer| {
                (peer.clone(), message.clone())
            }).collect();
        } else {
            vec![]
        }
    }
}