use std::cmp::{max, min};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use futures::prelude::*;
use futures::stream::futures_unordered;
use futures::sync::mpsc::{self, UnboundedReceiver as RX, UnboundedSender as TX};
use futures::sync::oneshot;
use futures_timer::Delay;
use labcodec;
use labrpc::RpcFuture;
use rand::prelude::*;
use tokio::spawn;

#[cfg(test)]
pub mod config;
pub mod errors;
pub mod persister;
pub mod service;
#[cfg(test)]
mod tests;

use self::errors::*;
use self::persister::*;
use self::service::*;

pub struct ApplyMsg {
    pub command_valid: bool,
    pub command: Vec<u8>,
    pub command_index: u64,
}

/// State of a raft peer.
#[derive(Default, Clone, Debug)]
pub struct State {
    pub term: u64,
    pub is_leader: bool,
}

impl State {
    /// The current term of this peer.
    pub fn term(&self) -> u64 {
        self.term
    }
    /// Whether this peer believes it is the leader.
    pub fn is_leader(&self) -> bool {
        self.is_leader
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Role {
    Leader,
    Candidate,
    Follower,
}

const ELECTION_TIMEOUT: std::ops::Range<u64> = 650..800;
const HEARTBEAT_TIMEOUT: u64 = 300;

// A single Raft peer.
pub struct Raft {
    apply_ch: TX<ApplyMsg>,
    // RPC end points of all peers
    peers: Vec<RaftClient>,
    // Object to hold this peer's persisted state
    persister: Box<dyn Persister>,
    // this peer's index into peers[]
    me: usize,
    role: Role,
    is_leader: Arc<AtomicBool>,
    // persistent state on allk servers
    current_term: Arc<AtomicU64>,
    voted_for: Option<usize>,
    log: Vec<Entry>,
    // volatile state on all servers
    commit_index: usize,
    last_applied: usize,
    applying: bool,
    // volatile state on leader
    next_index: Vec<usize>,
    match_index: Vec<usize>,
    // channels for events
    tx: TX<ActionEv>,
    rx: Option<RX<ActionEv>>,
}

use self::ActionEv::*;
use self::Event::*;
use self::TimeoutEv::*;

impl Raft {
    /// the service or tester wants to create a Raft server. the ports
    /// of all the Raft servers (including this one) are in peers. this
    /// server's port is peers[me]. all the servers' peers arrays
    /// have the same order. persister is a place for this server to
    /// save its persistent state, and also initially holds the most
    /// recent saved state, if any. apply_ch is a channel on which the
    /// tester or service expects Raft to send ApplyMsg messages.
    /// This method must return quickly.
    pub fn new(
        peers: Vec<RaftClient>,
        me: usize,
        persister: Box<dyn Persister>,
        apply_ch: TX<ApplyMsg>,
    ) -> Raft {
        let raft_state = persister.raft_state();

        let mut next_index = Vec::new();
        next_index.resize(peers.len(), 0);
        let match_index = next_index.clone();
        let (tx, rx) = mpsc::unbounded();
        let rx = Some(rx);

        // Your initialization code here (2A, 2B, 2C).
        let mut rf = Raft {
            apply_ch,
            peers,
            persister,
            me,
            role: Role::Follower,
            is_leader: Arc::default(),
            current_term: Arc::default(),
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            applying: false,
            next_index,
            match_index,
            tx,
            rx,
        };

        // initialize from state persisted before a crash
        rf.restore(&raft_state);

        rf
    }

    /// save Raft's persistent state to stable storage,
    /// where it can later be retrieved after a crash and restart.
    /// see paper's Figure 2 for a description of what should be persistent.
    #[allow(dead_code)]
    fn persist(&mut self) {
        // Your code here (2C).
        // Example:
        // labcodec::encode(&self.xxx, &mut data).unwrap();
        // labcodec::encode(&self.yyy, &mut data).unwrap();
        // self.persister.save_raft_state(data);
    }

    /// restore previously persisted state.
    fn restore(&mut self, data: &[u8]) {
        if data.is_empty() {
            // bootstrap without any state?
            return;
        }
        // Your code here (2C).
        // Example:
        // match labcodec::decode(data) {
        //     Ok(o) => {
        //         self.xxx = o.xxx;
        //         self.yyy = o.yyy;
        //     }
        //     Err(e) => {
        //         panic!("{:?}", e);
        //     }
        // }
    }

    /// example code to send a RequestVote RPC to a server.
    /// server is the index of the target server in peers.
    /// expects RPC arguments in args.
    ///
    /// The labrpc package simulates a lossy network, in which servers
    /// may be unreachable, and in which requests and replies may be lost.
    /// This method sends a request and waits for a reply. If a reply arrives
    /// within a timeout interval, This method returns Ok(_); otherwise
    /// this method returns Err(_). Thus this method may not return for a while.
    /// An Err(_) return can be caused by a dead server, a live server that
    /// can't be reached, a lost request, or a lost reply.
    ///
    /// This method is guaranteed to return (perhaps after a delay) *except* if
    /// the handler function on the server side does not return.  Thus there
    /// is no need to implement your own timeouts around this method.
    ///
    /// look at the comments in ../labrpc/src/mod.rs for more details.
    fn send_request_votes(&self) {
        let last_log_index = self.log.len() as u64;
        let last_log_term = self.log.last().map_or(0, |entry| entry.term);
        let term = self.current_term.load(Ordering::SeqCst);
        let args = RequestVoteArgs {
            term,
            candidate_id: self.me as u64,
            last_log_index,
            last_log_term,
        };

        let me = self.me;

        let reqs: Vec<_> = self
            .peers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != me)
            .map(|(i, peer)| {
                peer.request_vote(&args)
                    .map_err(move |e| (me, i, Error::Rpc(e)))
            })
            .collect();

        let stream = futures_unordered(reqs);
        let tx = self.tx.clone();
        let mut cnt = 1;
        let total = self.peers.len();

        spawn(
            stream
                .take_while(move |rep| {
                    if rep.vote_granted {
                        cnt += 1;
                        if cnt + cnt > total {
                            Self::report_event(tx.clone(), BecomeLeader(term));
                            Ok(false)
                        } else {
                            Ok(true)
                        }
                    } else if rep.term > term {
                        Self::report_event(tx.clone(), FoundNewTerm(rep.term));
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                })
                .map_err(|(me, id, e)| error!("{} -> {} RequestVote RPC error: {}", me, id, e))
                .for_each(|_| Ok(())),
        );
    }

    fn append_entries_args(&self, id: usize) -> AppendEntriesArgs {
        let next_idx = self.next_index[id];
        let match_idx = self.match_index[id];
        let pre_idx = next_idx - 1;
        let mut pre_term = 0;
        if pre_idx > 0 {
            pre_term = self.log[pre_idx-1].term
        }
        let entries = if next_idx > match_idx + 1 {
            Vec::new()
        } else {
            Vec::from(&self.log[next_idx - 1..])
        };
        AppendEntriesArgs {
            term: self.current_term.load(Ordering::SeqCst),
            leader_id: self.me as u64,
            prev_log_index: pre_idx as u64,
            prev_log_term: pre_term,
            entries,
            leader_commit: self.commit_index as u64,
        }
    }

    fn send_heartbeat(&self) {
        let term = self.current_term.load(Ordering::SeqCst);
        let me = self.me;
        let reqs: Vec<_> = self
            .peers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.me)
            .map(|(i, peer)| {
                let args = self.append_entries_args(i);
                peer.append_entries(&args)
                    .map_err(Error::Rpc)
                    .map_err(move |e| (me, i, e))
                    .map(move |rep| (rep, i, args.prev_log_index, args.entries.len()))
            })
            .collect();

        let stream = futures_unordered(reqs);
        let tx = self.tx.clone();

        spawn(
            stream
                .map_err(|(me, id, e)| error!("{} -> {} AppendEntries RPC error: {}", me, id, e))
                .for_each(move |(rep, id, pre_idx, len)| {
                    match rep.conflict {
                        None => {
                            // if success, return (term, id, match index)
                            Self::report_event(
                                tx.clone(),
                                UpdatePeer(term, id, Ok(pre_idx as usize + len)),
                            )
                        }
                        Some(conflict) => {
                            if rep.term > term {
                                Self::report_event(tx.clone(), FoundNewTerm(rep.term));
                            } else {
                                // if failed, return (term, id, next index)
                                // TODO
                                let nxt_idx = conflict.log_index as usize;
                                Self::report_event(tx.clone(), UpdatePeer(term, id, Err(nxt_idx)))
                            }
                        }
                    }
                    Ok(())
                }),
        );
    }

    fn report_event(tx: TX<ActionEv>, ev: ActionEv) {
        let ev_str = format!("{:?}", ev);
        spawn(tx.send(ev).map(|_| ()).map_err(move |e| {
            error!("failed to send {}: {}", ev_str, e);
        }));
    }

    /// return a Future which will be ready after election timeout.
    fn election_timeout(&self) -> Duration {
        let timeout = rand::thread_rng().gen_range(ELECTION_TIMEOUT.start, ELECTION_TIMEOUT.end);
        Duration::from_millis(timeout)
    }

    fn heartbeat_timeout(&self) -> Duration {
        let timeout = HEARTBEAT_TIMEOUT;
        Duration::from_millis(timeout)
    }

    /// check if self.log.last more up-to-date the the other
    fn is_newer(&self, term: u64, index: u64) -> bool {
        let last_log = match self.log.last() {
            Some(entry) => (entry.term, entry.index),
            None => (0, 0),
        };
        last_log > (term, index)
    }

    fn handle_request_vote(&mut self, args: RequestVoteArgs) -> RequestVoteReply {
        let log_term = args.last_log_term;
        let log_index = args.last_log_index;

        let term = self.current_term.load(Ordering::SeqCst);

        if args.term > term {
                self.current_term.store(args.term, Ordering::SeqCst);
        }

        let vote_granted = if args.term < term || self.is_newer(log_term, log_index) {
            false
        } else if args.term > term {
            true
        } else {
            match self.voted_for {
                Some(id) => id == args.candidate_id as usize,
                None => true,
            }
        };

        if vote_granted {
            self.voted_for = Some(args.candidate_id as usize);
        }

        RequestVoteReply { term, vote_granted }
    }

    fn is_conflict(&self, pre_index: usize, pre_term: u64) -> Option<EntryConflict> {
        if pre_index == 0 {
            if pre_term != 0 {
                Some(EntryConflict::new(0, 0))
            } else {
                None
            }
        } else {
            match self.log.get(pre_index - 1) {
                None => Some(EntryConflict::new(self.log.len()+1, 0)),
                Some(entry) => if entry.term != pre_term {
                    let mut idx = 1;
                    for i in (0..pre_index-1).rev() {
                        if self.log[i].term != entry.term {
                            idx = i + 2;
                        }
                    }
                    Some(EntryConflict::new(idx, entry.term))
                } else {
                    None
                }
            }
        }
    }

    fn handle_append_entries(&mut self, mut args: AppendEntriesArgs) -> AppendEntriesReply {
        let pre_idx = args.prev_log_index as usize;
        let pre_term = args.prev_log_term;

        let term = self.current_term.load(Ordering::SeqCst);
        let conflict = if args.term < term {
            Some(EntryConflict::new(0, 0))
        } else {
            self.is_conflict(pre_idx, pre_term)
        };

        if conflict.is_none() {
            if args.term > term {
                self.current_term.store(args.term, Ordering::SeqCst);
            }
            if let Some(entry) = args.entries.last() {
                // truncate and move if the last is conflict
                // TODO
                if self.is_conflict(entry.index as usize, entry.term).is_some() {
                    self.log.truncate(pre_idx);
                    self.log.append(&mut args.entries);
                }
            }
            if args.leader_commit > self.commit_index as u64 {
                self.commit_index = min(args.leader_commit as usize, pre_idx + args.entries.len());
            }
        }

        AppendEntriesReply { term, conflict }
    }

    fn become_leader(&mut self) {
        for i in 0..self.peers.len() {
            self.next_index[i] = self.log.len() + 1;
            self.match_index[i] = 0;
        }
        self.role = Role::Leader;
        self.is_leader.store(true, Ordering::SeqCst);
    }

    fn become_candidate(&mut self) {
        let term = self.current_term.load(Ordering::SeqCst) + 1;
        self.role = Role::Candidate;
        self.voted_for = Some(self.me);
        self.current_term.store(term, Ordering::SeqCst);

        self.send_request_votes();
    }

    fn become_follower(&mut self) {
        self.is_leader.store(false, Ordering::SeqCst);
        self.role = Role::Follower;
    }

    fn update_commit(&mut self, idx: usize) {
        if idx <= self.commit_index {
            return;
        }
        let term = self.log[idx - 1].term;
        if term == self.current_term.load(Ordering::SeqCst) {
            let sz = self.peers.len();
            let mut cnt = 1;
            let mut update = false;
            for i in 0..sz {
                if i != self.me && self.match_index[i] >= idx {
                    cnt += 1;
                    if cnt + cnt > sz {
                        update = true;
                        break;
                    }
                }
            }
            if update {
                self.commit_index = idx;
                self.apply_entry();
            }
        }
    }

    fn apply_entry(&mut self) {
        if self.commit_index <= self.last_applied || self.applying {
            return;
        }
        self.applying = true;
        let apply_idx = self.last_applied + 1;
        let msg = ApplyMsg {
            command_valid: true,
            command: self.log[apply_idx - 1].command.clone(),
            command_index: apply_idx as u64,
        };
        let ch = self.apply_ch.clone();
        let tx = self.tx.clone();
        let err_tx = tx.clone();
        spawn(
            ch.send(msg)
                .map(move |_| {
                    Self::report_event(tx, ApplySuccess);
                })
                .or_else(move |e| {
                    error!("apply entry failed: {}", e);
                    let delay = Delay::new(Duration::from_millis(70));
                    delay.map(|_| Self::report_event(err_tx, ApplyFailed)).map_err(|_| ())
                }),
        );
    }

    /// recursively spawn serve_loop
    fn serve_proc(mut self) -> impl FnMut(Event) -> ProcResult + Send + 'static {
        move |ev| {
            use self::ProcResult::*;
            match ev {
                Action(RequestVote(args, tx)) => {
                    debug!("{} handling RequestVote RPC: {:?}", self.me, args);
                    let rep = self.handle_request_vote(args);
                    let ok = rep.vote_granted;
                    debug!("{} reply for RequestVote RPC: {:?}", self.me, rep);
                    tx.send(rep)
                        .unwrap_or_else(|rep| error!("failed to send reply: {:?}", rep));
                    if ok {
                        self.become_follower();
                        let dur = self.election_timeout();
                        UpdateTimeout(dur, Election)
                    } else {
                        GoOn
                    }
                }
                Action(AppendEntries(args, tx)) => {
                    let args_term = args.term;
                    debug!("{} handling AppendEntries RPC: {:?}", self.me, args);
                    let rep = self.handle_append_entries(args);
                    let ok = rep.term <= args_term;
                    debug!("{} reply for AppendEntries RPC: {:?}", self.me, rep);
                    tx.send(rep)
                        .unwrap_or_else(|rep| error!("failed to send reply: {:?}", rep));
                    if ok {
                        self.apply_entry();
                        self.become_follower();
                        let dur = self.election_timeout();
                        UpdateTimeout(dur, Election)
                    } else {
                        GoOn
                    }
                }
                Action(FoundNewTerm(term)) => {
                    let current_term = self.current_term.load(Ordering::SeqCst);
                    if term > current_term {
                        self.become_follower();
                        let dur = self.election_timeout();
                        UpdateTimeout(dur, Election)
                    } else {
                        GoOn
                    }
                }
                Action(BecomeLeader(term)) => {
                    if term == self.current_term.load(Ordering::SeqCst) {
                        self.become_leader();
                        let dur = self.heartbeat_timeout();
                        UpdateTimeout(dur, Heartbeat)
                    } else {
                        info!("late BecomeLeader for term: {}", term);
                        GoOn
                    }
                }
                Action(UpdatePeer(term, id, res)) => {
                    if term == self.current_term.load(Ordering::SeqCst) {
                        match res {
                            Ok(match_idx) => {
                                let match_idx = max(self.match_index[id], match_idx);
                                self.match_index[id] = match_idx;
                                self.next_index[id] = match_idx + 1;
                                self.update_commit(match_idx);
                            }
                            Err(pre_idx) => {
                                self.next_index[id] = max(1, min(self.next_index[id], pre_idx));
                            }
                        }
                    } else {
                        info!("late BecomeLeader for term: {}", term);
                    }
                    ProcResult::GoOn
                }
                Action(NewEntry(cmd, tx)) => {
                    debug!("{} handling NewEntry: {:?}", self.me, cmd);
                    let res = self.start(cmd);
                    tx.send(res)
                        .unwrap_or_else(|res| error!("failed to send NewEntry reply: {:?}", res));
                    GoOn
                }
                Action(ApplySuccess) => {
                    self.last_applied += 1;
                    debug!("{} applying entry {} succeeded", self.me, self.last_applied);
                    self.applying = false;
                    self.apply_entry();
                    GoOn
                }
                Action(ApplyFailed) => {
                    warn!("{} applying entry {} failed", self.me, self.last_applied);
                    self.applying = false;
                    self.apply_entry();
                    GoOn
                }
                Action(Killed) => {
                    warn!("{} receive killed", self.me);
                    ProcResult::Stop
                }
                Timeout(Election) => {
                    self.become_candidate();
                    let dur = self.election_timeout();
                    UpdateTimeout(dur, Election)
                }
                Timeout(Heartbeat) => {
                    assert_eq!(self.role, Role::Leader);
                    self.send_heartbeat();
                    let dur = self.heartbeat_timeout();
                    UpdateTimeout(dur, Heartbeat)
                }
            }
        }
    }

    /// wait for RPCs
    /// if timeout become candidate
    fn serve(mut self) -> impl Future<Item = (), Error = ()> {
        futures::future::FutureResult::from(Ok(())).map(|_| {
            if let Some(rx) = self.rx.take() {
                let timeout = self.election_timeout();
                let evloop =
                    EventLoopFuture::new(rx, timeout, TimeoutEv::Election, self.serve_proc());
                spawn(evloop);
            } else {
                warn!("serve twice won't do anything")
            }
        })
    }

    fn start(&mut self, command: Vec<u8>) -> Result<(u64, u64)> {
        if !self.is_leader.load(Ordering::SeqCst) {
            return Err(Error::NotLeader);
        }
        let index = 1 + self.log.len() as u64;
        let term = self.current_term.load(Ordering::SeqCst);
        let entry = Entry {
            term,
            index,
            command,
        };
        self.log.push(entry);
        Ok((index, term))
    }
}

// Choose concurrency paradigm.
//
// You can either drive the raft state machine by the rpc framework,
//
// ```rust
// struct Node { raft: Arc<Mutex<Raft>> }
// ```
//
// or spawn a new thread runs the raft state machine and communicate via
// a channel.
//
// ```rust
// struct Node { sender: Sender<Msg> }
// ```
#[derive(Clone)]
pub struct Node {
    tx: TX<ActionEv>,
    term: Arc<AtomicU64>,
    is_leader: Arc<AtomicBool>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Node {
    /// Create a new raft service.
    pub fn new(raft: Raft) -> Node {
        let tx = raft.tx.clone();
        let term = raft.current_term.clone();
        let is_leader = raft.is_leader.clone();
        let handle = Some(thread::spawn(|| tokio::run(raft.serve())));
        let handle = Arc::new(Mutex::new(handle));
        Node {
            tx,
            term,
            is_leader,
            handle,
        }
    }

    /// the service using Raft (e.g. a k/v server) wants to start
    /// agreement on the next command to be appended to Raft's log. if this
    /// server isn't the leader, returns [`Error::NotLeader`]. otherwise start
    /// the agreement and return immediately. there is no guarantee that this
    /// command will ever be committed to the Raft log, since the leader
    /// may fail or lose an election. even if the Raft instance has been killed,
    /// this function should return gracefully.
    ///
    /// the first value of the tuple is the index that the command will appear
    /// at if it's ever committed. the second is the current term.
    ///
    /// This method must return without blocking on the raft.
    pub fn start<M>(&self, command: &M) -> Result<(u64, u64)>
    where
        M: labcodec::Message,
    {
        if !self.is_leader() {
            return Err(Error::NotLeader);
        }

        let mut buf = vec![];
        labcodec::encode(command, &mut buf).map_err(Error::Encode)?;

        let (tx, rx) = oneshot::channel();
        if let Err(e) = self.send(NewEntry(buf, tx)) {
            error!("failed to send NewEntry: {}", e);
            return Err(Error::NotLeader);
        }
        match rx.wait() {
            Ok(res) => res,
            Err(e) => {
                error!("failed to receive NewEntry reply: {}", e);
                Err(Error::NotLeader)
            }
        }
    }

    /// The current term of this peer.
    pub fn term(&self) -> u64 {
        self.term.load(Ordering::SeqCst)
    }

    /// Whether this peer believes it is the leader.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
    }

    /// The current state of this peer.
    pub fn get_state(&self) -> State {
        State {
            term: self.term(),
            is_leader: self.is_leader(),
        }
    }

    fn send(&self, ev: ActionEv) -> std::result::Result<(), String> {
        let mut tx = self.tx.clone().wait();
        if let Err(e) = tx.send(ev) {
            Err(e.to_string())
        } else {
            Ok(())
        }
    }

    /// the tester calls kill() when a Raft instance won't be
    /// needed again. you are not required to do anything in
    /// kill(), but it might be convenient to (for example)
    /// turn off debug output from this instance.
    /// In Raft paper, a server crash is a PHYSICAL crash,
    /// A.K.A all resources are reset. But we are simulating
    /// a VIRTUAL crash in tester, so take care of background
    /// threads you generated with this Raft Node.
    pub fn kill(&self) {
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            if let Err(e) = self.send(Killed) {
                error!("send Killed failed: {}", e);
            }
            handle.join().unwrap();
        } else {
            error!("cannot kill a node twice");
        }
    }
}

impl RaftService for Node {
    // example RequestVote RPC handler.
    fn request_vote(&self, args: RequestVoteArgs) -> RpcFuture<RequestVoteReply> {
        let (tx, rx) = oneshot::channel();
        Box::new(
            self.tx
                .clone()
                .send(RequestVote(args, tx))
                .map_err(|e| {
                    let err = format!("failed to send RequestVote to raft: {}", e);
                    labrpc::Error::Other(err)
                })
                .and_then(move |_| {
                    rx.map_err(|_| {
                        let err = "failed to receive RequestVoteReply from raft".to_string();
                        labrpc::Error::Other(err)
                    })
                }),
        )
    }

    fn append_entries(&self, args: AppendEntriesArgs) -> RpcFuture<AppendEntriesReply> {
        let (tx, rx) = oneshot::channel();
        Box::new(
            self.tx
                .clone()
                .send(AppendEntries(args, tx))
                .map_err(|e| {
                    let err = format!("failed to send AppendEntries to raft: {}", e);
                    labrpc::Error::Other(err)
                })
                .and_then(move |_| {
                    rx.map_err(|_| {
                        let err = "failed to receive AppendEntriesReply from raft".to_string();
                        labrpc::Error::Other(err)
                    })
                }),
        )
    }
}

#[derive(Debug)]
enum Event {
    Action(ActionEv),
    Timeout(TimeoutEv),
}

#[derive(Debug)]
enum ActionEv {
    RequestVote(RequestVoteArgs, oneshot::Sender<RequestVoteReply>),
    AppendEntries(AppendEntriesArgs, oneshot::Sender<AppendEntriesReply>),
    FoundNewTerm(u64),
    BecomeLeader(u64),
    UpdatePeer(u64, usize, std::result::Result<usize, usize>),
    NewEntry(Vec<u8>, oneshot::Sender<Result<(u64, u64)>>),
    ApplySuccess,
    ApplyFailed,
    Killed,
}

#[derive(Debug, Clone)]
enum TimeoutEv {
    Election,
    Heartbeat,
}

enum ProcResult {
    UpdateTimeout(Duration, TimeoutEv),
    GoOn,
    Stop,
}

struct EventLoopFuture<F>
where
    F: FnMut(Event) -> ProcResult + Send + 'static,
{
    rx: RX<ActionEv>,
    default_dur: Duration,
    timeout: Delay,
    timeout_ev: TimeoutEv,
    proc: F,
    running: bool,
}

impl<F> EventLoopFuture<F>
where
    F: FnMut(Event) -> ProcResult + Send + 'static,
{
    fn new(
        rx: RX<ActionEv>,
        timeout: Duration,
        timeout_ev: TimeoutEv,
        proc: F,
    ) -> EventLoopFuture<F> {
        let default_dur = timeout;
        let timeout = Delay::new(timeout);

        EventLoopFuture {
            rx,
            default_dur,
            timeout,
            timeout_ev,
            proc,
            running: true,
        }
    }
}

impl<F> Future for EventLoopFuture<F>
where
    F: FnMut(Event) -> ProcResult + Send + 'static,
{
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Poll<(), ()> {
        while self.running {
            match self.rx.poll() {
                Ok(Async::Ready(Some(ev))) => {
                    use self::ProcResult::*;
                    match (self.proc)(Event::Action(ev)) {
                        UpdateTimeout(dur, ev) => {
                            self.timeout.reset(dur);
                            self.timeout_ev = ev;
                        }
                        Stop => {
                            self.running = false;
                        }
                        GoOn => {}
                    }
                    continue;
                }
                Ok(Async::Ready(None)) => {
                    info!("event loop end");
                    return Ok(Async::Ready(()));
                }
                Ok(Async::NotReady) => {}
                Err(()) => unreachable!(),
            }
            match self.timeout.poll() {
                Ok(Async::Ready(())) => {
                    use self::ProcResult::*;
                    let ev = Timeout(self.timeout_ev.clone());
                    match (self.proc)(ev) {
                        UpdateTimeout(dur, ev) => {
                            self.timeout.reset(dur);
                            self.timeout_ev = ev;
                        }
                        GoOn => {
                            self.timeout.reset(self.default_dur);
                        }
                        Stop => {
                            self.running = false;
                        }
                    }
                }
                Ok(Async::NotReady) => {
                    return Ok(Async::NotReady);
                }
                Err(e) => {
                    error!("timeout error: {}", e);
                    self.timeout = Delay::new(self.default_dur);
                }
            }
        }
        info!("event loop end");
        Ok(Async::Ready(()))
    }
}
