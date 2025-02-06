use std::array;

use tokio::{
    sync::watch,
    time::{Duration, Instant},
};
use tracing::Span;

use crate::{
    network::{IncomingMessage, NodeId},
    raft::{logic::RaftState, RaftLeader, RaftMessage},
};

#[tokio::test]
async fn should_elect_self_as_leader() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(peers, vec![other_id1.clone(), other_id2.clone()]);
    assert_eq!(term, 1);

    // Once a single node votes for us, we have reached a quorum of 2 and are the leader
    assert_eq!(
        state.receive(
            &other_id1,
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        ),
        [
            (other_id1.clone(), RaftMessage::Heartbeat { term: 1 }),
            (other_id2.clone(), RaftMessage::Heartbeat { term: 1 }),
        ]
    );

    assert_eq!(leader_source.borrow().clone(), RaftLeader::Myself);
}

#[tokio::test]
async fn should_not_count_false_votes() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(peers, vec![other_id1.clone(), other_id2.clone()]);
    assert_eq!(term, 1);

    // If a node refuses to connect to us, we shouldn't count their vote
    assert_eq!(
        state.receive(
            &other_id1,
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: false
            }
        ),
        vec![]
    );
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Unknown);
}

#[tokio::test]
async fn should_not_double_count_votes_from_one_node() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2, other_id3], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id3, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(
        peers,
        vec![other_id1.clone(), other_id2.clone(), other_id3.clone()]
    );
    assert_eq!(term, 1);

    // With four nodes, we need 3 votes (including our own) to reach quorum.
    // One node voting for us is not enough.
    assert_eq!(
        state.receive(
            &other_id1,
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        ),
        vec![]
    );
    // The same node voting twice is still not enough.
    assert_eq!(
        state.receive(
            &other_id1,
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        ),
        vec![]
    );
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Unknown);
}

#[tokio::test]
async fn should_vote_for_other_node() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    // We vote for the first candidate who asks us to
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        )]
    );
}

#[tokio::test]
async fn should_not_vote_twice_in_one_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    // We vote for the first candidate who asks us to
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        )]
    );
    // We DON'T vote for the second
    assert_eq!(
        state.receive(&other_id2, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id2.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: false
            }
        )]
    );
}

#[tokio::test]
async fn should_vote_again_for_second_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    // We vote for the first candidate who asks us to
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        )]
    );
    // But if someone else comes along with a newer term, vote for them instead
    assert_eq!(
        state.receive(&other_id2, RaftMessage::RequestVote { term: 10 }),
        vec![(
            other_id2.clone(),
            RaftMessage::RequestVoteResponse {
                term: 10,
                vote: true
            }
        )]
    );
}

#[tokio::test]
async fn should_return_new_term_when_vote_requested_for_older_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    // get ourself onto a high term number
    state.become_follower(&other_id1, 1000);
    assert_eq!(state.receive(&other_id1, RaftMessage::Disconnect), vec![]);
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Unknown);

    // If a candidate on an older term asks for a vote, we should respond with our actual term
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1000,
                vote: true
            }
        )]
    );
}

#[tokio::test]
async fn should_update_term_when_vote_received_for_newer_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(peers, vec![other_id1.clone(), other_id2.clone()]);
    assert_eq!(term, 1);

    assert_eq!(
        state.receive(
            &other_id2,
            RaftMessage::RequestVoteResponse {
                term: 1000,
                vote: true
            }
        ),
        vec![
            (other_id1.clone(), RaftMessage::Heartbeat { term: 1000 }),
            (other_id2.clone(), RaftMessage::Heartbeat { term: 1000 }),
        ]
    );
}

#[tokio::test]
async fn should_update_term_when_false_vote_received_for_newer_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(peers, vec![other_id1.clone(), other_id2.clone()]);
    assert_eq!(term, 1);

    assert_eq!(
        state.receive(
            &other_id2,
            RaftMessage::RequestVoteResponse {
                term: 1000,
                vote: false
            }
        ),
        vec![]
    );

    let (_, next_term) = state.begin_election();
    assert_eq!(next_term, 1001);
}

#[tokio::test]
async fn should_respond_idempotently_to_request_for_vote() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    // Voting for the first node
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        )]
    );
    // Even if they ask more than once
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 1 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 1,
                vote: true
            }
        )]
    );
}

#[tokio::test]
async fn should_renounce_leadership_when_vote_requested_from_newer_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_leader();
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Myself);
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 100 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 100,
                vote: true
            }
        )]
    );
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Unknown);
}

#[tokio::test]
async fn should_consider_other_node_leader_when_heartbeat_received() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    assert_eq!(
        state.receive(&other_id1, RaftMessage::Heartbeat { term: 1 }),
        vec![]
    );
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Other(other_id1));
}

#[tokio::test]
async fn should_renounce_leadership_when_quorum_lost() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_leader();
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Myself);

    // Disconnecting from one node does not make us lose quorum
    assert_eq!(state.receive(&other_id2, RaftMessage::Disconnect), vec![]);
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Myself);

    // But disconnecting from a second node does
    assert_eq!(state.receive(&other_id1, RaftMessage::Disconnect), vec![]);
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Unknown);
}

#[tokio::test]
async fn should_renounce_leadership_when_heartbeat_received_from_node_with_higher_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_leader();
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Myself);

    assert_eq!(
        state.receive(&other_id1, RaftMessage::Heartbeat { term: 9001 }),
        vec![]
    );
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Other(other_id1));
}

#[tokio::test]
async fn should_ignore_leader_heartbeats_from_lower_term() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_follower(&other_id1, 9001);
    assert_eq!(
        leader_source.borrow().clone(),
        RaftLeader::Other(other_id1.clone())
    );

    assert_eq!(
        state.receive(&other_id2, RaftMessage::Heartbeat { term: 1337 }),
        vec![]
    );
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Other(other_id1));
}

#[tokio::test]
async fn should_notify_new_nodes_when_we_are_leader() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);

    state.become_leader();
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Myself);

    assert_eq!(
        state.receive(&other_id2, RaftMessage::Connect),
        vec![(other_id2.clone(), RaftMessage::Heartbeat { term: 1 })]
    );
}

#[tokio::test]
async fn should_forget_current_leader_if_they_disconnect() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], leader_source) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_follower(&other_id1, 2);
    assert_eq!(
        leader_source.borrow().clone(),
        RaftLeader::Other(other_id1.clone())
    );

    assert_eq!(state.receive(&other_id1, RaftMessage::Disconnect), vec![]);
    assert_eq!(leader_source.borrow().clone(), RaftLeader::Unknown);
}

#[tokio::test]
async fn should_start_new_election_if_current_election_times_out() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(peers, vec![other_id1.clone(), other_id2.clone()]);
    assert_eq!(term, 1);

    assert_eq!(
        state.tick(timeout_freq),
        vec![
            (other_id1.clone(), RaftMessage::RequestVote { term: 2 }),
            (other_id2.clone(), RaftMessage::RequestVote { term: 2 }),
        ]
    );
}

#[tokio::test]
async fn should_delay_starting_new_election_if_missing_payloads() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_follower(&other_id1, 1);

    state.set_missing_payloads(2);

    // we're missing two payloads, so we should wait two timeout cycles before running for leader.
    assert_eq!(state.tick(timeout_freq), vec![]);
    assert_eq!(state.tick(timeout_freq), vec![]);
    assert_eq!(
        state.tick(timeout_freq),
        vec![
            (other_id1.clone(), RaftMessage::RequestVote { term: 2 }),
            (other_id2.clone(), RaftMessage::RequestVote { term: 2 }),
        ]
    );
}

#[tokio::test]
async fn should_stop_sending_messages_if_we_abdicate() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2, other_id3], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);
    let term = state.become_leader();

    // While we're leader, we send heartbeats
    assert_eq!(
        state.tick(heartbeat_freq),
        vec![
            (other_id1, RaftMessage::Heartbeat { term }),
            (other_id2, RaftMessage::Heartbeat { term })
        ]
    );

    // Once we're not, we stop heartbeating
    state.abdicate();
    assert_eq!(state.tick(heartbeat_freq), vec![]);
    // And also don't run in the next election
    assert_eq!(state.tick(timeout_freq), vec![]);
    // And also don't tell newcomers that we're leader
    assert_eq!(state.receive(&other_id3, RaftMessage::Connect), vec![]);
}

#[tokio::test]
async fn should_reset_new_election_timeout_after_voting() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    let (peers, term) = state.begin_election();
    assert_eq!(peers, vec![other_id1.clone(), other_id2.clone()]);
    assert_eq!(term, 1);

    // run for a while, but not long enough to time out
    assert_eq!(
        state.tick(timeout_freq / 2 + Duration::from_millis(500)),
        vec![]
    );
    // Oh, we voted before the timeout hit
    assert_eq!(
        state.receive(&other_id1, RaftMessage::RequestVote { term: 20 }),
        vec![(
            other_id1.clone(),
            RaftMessage::RequestVoteResponse {
                term: 20,
                vote: true
            }
        )]
    );

    // now we run for long enough to trigger the old timeout, and confirm it didn't get triggered
    assert_eq!(state.tick(timeout_freq / 2), vec![]);
    // but if we run for a little longer, we hit the timeout and start a new election
    assert_eq!(
        state.tick(timeout_freq / 2),
        vec![
            (other_id1.clone(), RaftMessage::RequestVote { term: 21 }),
            (other_id2.clone(), RaftMessage::RequestVote { term: 21 }),
        ]
    );
}

#[tokio::test]
async fn should_not_reset_new_election_timeout_after_receiving_old_heartbeat() {
    let start_time = Instant::now();
    let heartbeat_freq = Duration::from_millis(1000);
    let timeout_freq = Duration::from_millis(2000);

    let (mut state, [other_id1, other_id2], _) =
        generate_state(start_time, heartbeat_freq, timeout_freq);
    assert_eq!(state.receive(&other_id1, RaftMessage::Connect), vec![]);
    assert_eq!(state.receive(&other_id2, RaftMessage::Connect), vec![]);

    state.become_follower(&other_id1, 10);

    // run for a while, but not long enough to time out
    assert_eq!(
        state.tick(timeout_freq / 2 + Duration::from_millis(500)),
        vec![]
    );

    // receive a heartbeat from node 2, it thinks it's leader but its vote is too old
    assert_eq!(
        state.receive(&other_id2, RaftMessage::Heartbeat { term: 5 }),
        []
    );

    // now if we run for long enough to trigger the old timeout, it should still get triggered
    assert_eq!(
        state.tick(timeout_freq / 2),
        vec![
            (other_id1.clone(), RaftMessage::RequestVote { term: 11 }),
            (other_id2.clone(), RaftMessage::RequestVote { term: 11 }),
        ]
    );
}

fn generate_state<const N: usize>(
    start_time: Instant,
    heartbeat_freq: Duration,
    timeout_freq: Duration,
) -> (Participant, [NodeId; N], watch::Receiver<RaftLeader>) {
    let (leader_sink, leader_source) = watch::channel(RaftLeader::Unknown);
    let quorum = ((N + 1) / 2) + 1;
    let my_id = NodeId::new("me".into());
    let participant = Participant {
        now: start_time,
        timeout_freq,
        state: RaftState::new(
            my_id,
            start_time,
            quorum,
            heartbeat_freq,
            timeout_freq,
            leader_sink,
        ),
    };
    let other_ids = array::from_fn(|idx| NodeId::new(idx.to_string()));
    (participant, other_ids, leader_source)
}

struct Participant {
    now: Instant,
    timeout_freq: Duration,
    state: RaftState,
}
impl Participant {
    pub fn receive(&mut self, from: &NodeId, message: RaftMessage) -> Vec<(NodeId, RaftMessage)> {
        self.now += Duration::from_millis(1);
        self.state.receive(
            self.now,
            IncomingMessage {
                from: from.clone(),
                data: message,
                span: Span::none(),
            },
        )
    }

    pub fn tick(&mut self, duration: Duration) -> Vec<(NodeId, RaftMessage)> {
        self.now += duration;
        self.state.tick(self.now)
    }

    pub fn abdicate(&mut self) {
        self.state.abdicate();
    }

    pub fn set_missing_payloads(&mut self, missing_payloads: usize) {
        self.state.set_missing_payloads(missing_payloads);
    }

    pub fn begin_election(&mut self) -> (Vec<NodeId>, usize) {
        // "run" for long enough to start another election
        let begin_election_messages = self.tick(self.timeout_freq);
        if begin_election_messages.is_empty() {
            panic!("We tried initiating an election, but failed");
        }
        let mut peers = vec![];
        let mut term = 0;
        for (from, message) in begin_election_messages {
            let RaftMessage::RequestVote { term: t } = message else {
                panic!("We tried initiating an election, but failed");
            };
            peers.push(from);
            term = t;
        }
        (peers, term)
    }

    pub fn become_leader(&mut self) -> usize {
        // "run" for long enough to start another election
        let (peers, term) = self.begin_election();
        for peer in peers {
            // have the others vote for us until we are leader
            let heartbeat_messages =
                self.receive(&peer, RaftMessage::RequestVoteResponse { term, vote: true });
            if !heartbeat_messages.is_empty() {
                // We know that we are the leader if we sent out heartbeats to every other node
                assert!(heartbeat_messages
                    .iter()
                    .all(|(_, message)| matches!(message, RaftMessage::Heartbeat { .. })));
                return term;
            }
        }
        panic!("Failed to become leader");
    }

    pub fn become_follower(&mut self, new_leader: &NodeId, term: usize) {
        assert_eq!(
            self.receive(new_leader, RaftMessage::Heartbeat { term }),
            vec![]
        );
    }
}
