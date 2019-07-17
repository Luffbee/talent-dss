labrpc::service! {
    service raft {
        rpc request_vote(RequestVoteArgs) returns (RequestVoteReply);
        rpc append_entries(AppendEntriesArgs) returns (AppendEntriesReply);

        // Your code here if more rpc desired.
        // rpc xxx(yyy) returns (zzz)
    }
}
pub use self::raft::{
    add_service as add_raft_service, Client as RaftClient, Service as RaftService,
};

/// Example RequestVote RPC arguments structure.
#[derive(Clone, PartialEq, Message)]
pub struct RequestVoteArgs {
    #[prost(uint64, tag=1)]
    pub term: u64,
    #[prost(uint64)]
    pub candidate_id: u64,
    #[prost(uint64)]
    pub last_log_index: u64,
    #[prost(uint64)]
    pub last_log_term: u64,
}

// Example RequestVote RPC reply structure.
#[derive(Clone, PartialEq, Message)]
pub struct RequestVoteReply {
    #[prost(uint64, tag=1)]
    pub term: u64,
    #[prost(bool)]
    pub vote_granted: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Entry {
    #[prost(uint64, tag=1)]
    pub term: u64,
    #[prost(uint64)]
    pub index: u64,
    #[prost(bytes)]
    pub command: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppendEntriesArgs {
    #[prost(uint64, tag=1)]
    pub term: u64,
    #[prost(uint64)]
    pub leader_id: u64,
    #[prost(uint64)]
    pub prev_log_index: u64,
    #[prost(uint64)]
    pub prev_log_term: u64,
    #[prost(message, repeated)]
    pub entries: Vec<Entry>,
    #[prost(uint64)]
    pub leader_commit: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct AppendEntriesReply {
    #[prost(uint64, tag=1)]
    pub term: u64,
    #[prost(bool)]
    pub success: bool,
}
