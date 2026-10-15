// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

/// Custom errors of WerewolfEscrow; no imports, so Rust `sol!` can load this file alone.
interface IWerewolfErrors {
    error NotOwner();
    error NotOperator();
    error NotProvider();
    error NotSeated();
    error IsPaused();
    error InvalidParams();
    /// `status` is an `IWerewolfEscrow.Status`.
    error WrongStatus(uint8 status);
    error TooLate();
    error GameFull();
    error AlreadySeated();
    error InvalidSessionKey();
    error NotEnoughSeats();
    error AlreadyFulfilled();
    error NotFulfilled();
    error WrongRound();
    error InvalidPayload();
    error InvalidSeed();
    error InvalidResult();
    error AlreadyProven();
    error InvalidSeat();
    error InvalidSignature();
    error DisputeAlreadyOpen();
    error DisputeNotOpen();
    error InvalidMerkleProof();
    error PublicValuesMismatch();
    error NotFinalizable();
    error NothingToClaim();
}
