//! turn 启动、条件 steering 与中断。

use std::sync::atomic::Ordering;

use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::Server;
use crate::ThreadWorkerMsg;
use crate::Turn;
use crate::params::MethodResult;
use crate::params::SteerError;
use crate::params::parse_input;
use crate::params::str_param;
use crate::wire;

impl Server {
    pub(crate) fn turn_start(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "thread_id")?;
        let (text, images) = parse_input(params)?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            (
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            )
        })?;
        if handle.running.swap(true, Ordering::SeqCst) {
            return Err((
                wire::SERVER_ERROR,
                format!("a turn is already running on thread '{thread_id}'"),
            ));
        }
        // Allocate the turn id and publish it before the worker picks up the
        // turn, so a `turn/steer` racing in sees the running turn's id.
        let turn_id = handle.turn_seq.fetch_add(1, Ordering::SeqCst);
        *handle.turn.lock().unwrap() = Some(turn_id);
        handle.inbox.open_steer_window(turn_id);
        let cancel = CancellationToken::new();
        *handle.current_cancel.lock().unwrap() = Some(cancel.clone());
        handle
            .projection
            .record_input(Some(turn_id), &params["input"]);
        if handle
            .turn_tx
            .send(ThreadWorkerMsg::Turn(Turn {
                text,
                images,
                id: turn_id,
                cancel,
                delivery_only: false,
            }))
            .is_err()
        {
            handle.inbox.close_steer_window();
            handle.running.store(false, Ordering::SeqCst);
            *handle.current_cancel.lock().unwrap() = None;
            *handle.turn.lock().unwrap() = None;
            handle.projection.discard_turn(turn_id);
            return Err((wire::SERVER_ERROR, "thread worker is gone".into()));
        }
        Ok(json!({"turn": {"id": turn_id}}))
    }

    /// Enqueue steering text typed while a turn runs (or between turns). Unlike
    /// `turn/start` this never starts a turn itself and never checks the
    /// running flag: a steer admitted into a running turn's window folds in at
    /// its next round boundary, and one pushed while idle wakes the worker's
    /// inbox branch, which starts a delivery turn for it.
    ///
    /// `expected_turn_id` makes the steer conditional: it is enqueued only while
    /// that turn is still taking steers, and otherwise refused with nothing
    /// enqueued — so a steer meant to amend a turn that has already finished is
    /// never delivered as the start of a new one. Returns the turn the steer
    /// will reach, or null when a later turn will deliver it; the answer is
    /// read under the same lock the turn's final drain closes the window with,
    /// so it is never a turn that has already stopped looking.
    pub(crate) fn turn_steer(&mut self, params: &Value) -> Result<Value, SteerError> {
        let thread_id = str_param(params, "thread_id").map_err(SteerError::Request)?;
        let expected = match params.get("expected_turn_id") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                SteerError::Request((
                    wire::INVALID_PARAMS,
                    "expected_turn_id must be a non-negative integer".into(),
                ))
            })?),
        };
        let (text, _images) = parse_input(params).map_err(SteerError::Request)?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            SteerError::Request((
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            ))
        })?;
        let input = Value::String(text.clone());
        let turn_id = handle
            .inbox
            .push_steer(text, expected)
            .map_err(SteerError::Refused)?;
        handle.projection.record_input(turn_id, &input);
        Ok(json!({"turn_id": turn_id}))
    }

    pub(crate) fn turn_interrupt(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "thread_id")?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            (
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            )
        })?;
        // An interrupted turn stops draining before it stops running; a steer
        // naming it from here on must not be acknowledged as reaching it.
        handle.inbox.close_steer_window();
        if let Some(cancel) = handle.current_cancel.lock().unwrap().as_ref() {
            cancel.cancel();
        }
        Ok(json!({}))
    }
}
