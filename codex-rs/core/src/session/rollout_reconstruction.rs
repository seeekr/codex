use super::*;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::is_user_turn_boundary;
use codex_protocol::protocol::CorrectionIntent;
use codex_protocol::protocol::SessionContextWindow;
use std::collections::BTreeSet;
use uuid::Uuid;

// Return value of `Session::reconstruct_history_from_rollout`, bundling the rebuilt history with
// the resume/fork hydration metadata derived from the same replay.
#[derive(Debug)]
pub(super) struct RolloutReconstruction {
    pub(super) history: Vec<ResponseItem>,
    pub(super) surviving_client_user_message_ids: HashSet<String>,
    pub(super) correction_intents: Vec<CorrectionIntent>,
    pub(super) correction_receipts: BTreeMap<String, ResponseItem>,
    pub(super) correction_receipt_conflicts: HashSet<String>,
    pub(super) correction_integrity_failure: Option<String>,
    pub(super) processed_correction_ids: HashSet<String>,
    pub(super) correction_auto_start_suppressed: bool,
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Option<Uuid>,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy)]
struct ReconstructedWindow {
    number: u64,
    first_id: Option<Uuid>,
    previous_id: Option<Uuid>,
    id: Option<Uuid>,
}

#[derive(Debug, Default)]
enum TurnReferenceContextItem {
    /// No `TurnContextItem` has been seen for this replay span yet.
    ///
    /// This differs from `Cleared`: `NeverSet` means there is no evidence this turn ever
    /// established a baseline, while `Cleared` means a baseline existed and a later compaction
    /// invalidated it. Only the latter must emit an explicit clearing segment for resume/fork
    /// hydration.
    #[default]
    NeverSet,
    /// A previously established baseline was invalidated by later compaction.
    Cleared,
    /// The latest baseline established by this replay span.
    Latest(Box<TurnContextItem>),
}

#[derive(Debug, Default)]
struct ActiveReplaySegment<'a> {
    turn_id: Option<String>,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay: Vec<&'a RolloutItem>,
    base_replacement_history: Option<&'a [ResponseItem]>,
    window: Option<ReconstructedWindow>,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn finalize_active_segment<'a>(
    active_segment: ActiveReplaySegment<'a>,
    base_replacement_history: &mut Option<&'a [ResponseItem]>,
    previous_turn_settings: &mut Option<PreviousTurnSettings>,
    reference_context_item: &mut TurnReferenceContextItem,
    world_state_replay: &mut Vec<&'a RolloutItem>,
    window: &mut Option<ReconstructedWindow>,
    pending_rollback_turns: &mut usize,
) {
    // Thread rollback drops the newest surviving real user-message boundaries. In replay, that
    // means skipping the next finalized segments that contain a non-contextual
    // `EventMsg::UserMessage`.
    if *pending_rollback_turns > 0 {
        if active_segment.counts_as_user_turn {
            *pending_rollback_turns -= 1;
        }
        return;
    }

    world_state_replay.extend(active_segment.world_state_replay);

    // A surviving replacement-history checkpoint is a complete history base. Once we
    // know the newest surviving one, older rollout items do not affect rebuilt history.
    if base_replacement_history.is_none()
        && let Some(segment_base_replacement_history) = active_segment.base_replacement_history
    {
        *base_replacement_history = Some(segment_base_replacement_history);
    }

    if window.is_none() {
        *window = active_segment.window;
    }

    // `previous_turn_settings` come from the newest surviving user turn that established them.
    if previous_turn_settings.is_none() && active_segment.counts_as_user_turn {
        *previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving user turn baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        && (active_segment.counts_as_user_turn
            || matches!(
                active_segment.reference_context_item,
                TurnReferenceContextItem::Cleared
            ))
    {
        *reference_context_item = active_segment.reference_context_item;
    }
}

/// Reconstruct the correction receipt ledger independently from prompt-history compaction.
///
/// A receipt belongs to the latest surviving real user/instruction turn. Rollback removes the
/// newest segments and makes the preceding surviving segment current again; only a correction
/// with no surviving turn is standalone. This mirrors prompt-tail rollback without letting
/// compaction erase the idempotency ledger.
pub(super) fn reconstruct_correction_receipts(
    rollout_items: &[RolloutItem],
) -> (BTreeMap<String, ResponseItem>, HashSet<String>) {
    let mut standalone = Vec::new();
    let mut turn_segments: Vec<Vec<ResponseItem>> = Vec::new();
    let mut current_segment = None;

    for item in rollout_items {
        match item {
            RolloutItem::ResponseItem(response_item) => {
                if is_user_turn_boundary(response_item) {
                    turn_segments.push(Vec::new());
                    current_segment = Some(turn_segments.len() - 1);
                }
                if super::correction::correction_frame_id(response_item).is_some() {
                    if let Some(segment) =
                        current_segment.and_then(|index| turn_segments.get_mut(index))
                    {
                        segment.push(response_item.clone());
                    } else {
                        standalone.push(response_item.clone());
                    }
                }
            }
            RolloutItem::InterAgentCommunication(_) => {
                turn_segments.push(Vec::new());
                current_segment = Some(turn_segments.len() - 1);
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let drop_count = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                turn_segments.truncate(turn_segments.len().saturating_sub(drop_count));
                current_segment = turn_segments.len().checked_sub(1);
            }
            RolloutItem::Compacted(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
        }
    }

    let mut receipts = BTreeMap::new();
    let mut conflicts = HashSet::new();
    for frame in standalone
        .into_iter()
        .chain(turn_segments.into_iter().flatten())
    {
        let Some(stable_id) = super::correction::correction_frame_id(&frame) else {
            continue;
        };
        match receipts.get(stable_id) {
            Some(existing)
                if !super::correction::correction_frames_have_same_payload(existing, &frame) =>
            {
                tracing::error!(
                    correction_id = stable_id,
                    "conflicting durable correction frames found during rollout replay"
                );
                conflicts.insert(stable_id.to_string());
            }
            Some(_) => {}
            None => {
                receipts.insert(stable_id.to_string(), frame);
            }
        }
    }
    (receipts, conflicts)
}

type ReconstructedCorrectionState = (
    Vec<CorrectionIntent>,
    BTreeMap<String, ResponseItem>,
    HashSet<String>,
    HashSet<String>,
    Option<String>,
);

fn reconstruct_correction_state(
    rollout_items: &[RolloutItem],
    surviving_client_user_message_ids: &HashSet<String>,
) -> ReconstructedCorrectionState {
    let (legacy_receipts, legacy_conflicts) = reconstruct_correction_receipts(rollout_items);
    let mut intent_by_id = BTreeMap::<String, CorrectionIntent>::new();
    let mut intent_order = Vec::<String>::new();
    let mut typed_ids = HashSet::<String>::new();
    let mut conflicts = HashSet::<String>::new();
    let mut integrity_failures = BTreeSet::<String>::new();

    for item in rollout_items {
        let RolloutItem::EventMsg(EventMsg::RawResponseItem(event)) = item else {
            continue;
        };
        let Some(intent) = event.persisted_correction_intent() else {
            continue;
        };
        let Ok(expected_frame) =
            Session::validate_correction(&intent.correction_id, intent.payload.clone())
        else {
            tracing::warn!(
                correction_id = intent.correction_id,
                "ignored invalid durable correction intent during rollout replay"
            );
            integrity_failures.insert("invalid durable intent".to_string());
            continue;
        };
        let Some(stable_id) = correction::correction_frame_id(&expected_frame).map(str::to_string)
        else {
            integrity_failures.insert("invalid durable intent".to_string());
            continue;
        };
        let Ok(correction_id) = Uuid::parse_str(&intent.correction_id) else {
            integrity_failures.insert("invalid durable intent".to_string());
            continue;
        };
        let canonical_intent = CorrectionIntent {
            correction_id: correction_id.to_string(),
            expected_client_user_message_id: intent.expected_client_user_message_id.clone(),
            payload: intent.payload.clone(),
        };
        typed_ids.insert(stable_id.clone());
        match intent_by_id.get(&stable_id) {
            Some(existing) if existing != &canonical_intent => {
                integrity_failures
                    .insert("correction identity reused with different data".to_string());
                conflicts.insert(stable_id);
            }
            Some(_) => {}
            None => {
                intent_by_id.insert(stable_id.clone(), canonical_intent);
                intent_order.push(stable_id);
            }
        }
    }

    let correction_intents = intent_order
        .iter()
        .filter_map(|stable_id| intent_by_id.get(stable_id).cloned())
        .collect::<Vec<_>>();
    let surviving_by_id = correction_intents
        .iter()
        .filter(|intent| {
            surviving_client_user_message_ids.contains(&intent.expected_client_user_message_id)
        })
        .filter_map(|intent| {
            let frame =
                Session::validate_correction(&intent.correction_id, intent.payload.clone()).ok()?;
            Some((
                correction::correction_frame_id(&frame)?.to_string(),
                (intent, frame),
            ))
        })
        .collect::<BTreeMap<_, _>>();

    let mut receipts = legacy_receipts
        .into_iter()
        .filter(|(stable_id, _)| !typed_ids.contains(stable_id))
        .collect::<BTreeMap<_, _>>();
    conflicts.extend(
        legacy_conflicts
            .into_iter()
            .filter(|stable_id| !typed_ids.contains(stable_id)),
    );
    if !conflicts.is_empty() {
        integrity_failures.insert("conflicting durable correction frame".to_string());
    }
    for item in rollout_items {
        let RolloutItem::ResponseItem(frame) = item else {
            continue;
        };
        let Some(stable_id) = correction::correction_frame_id(frame) else {
            continue;
        };
        if !typed_ids.contains(stable_id) {
            continue;
        }
        let Some((_, expected_frame)) = surviving_by_id.get(stable_id) else {
            continue;
        };
        if conflicts.contains(stable_id) {
            continue;
        }
        if !correction::correction_frames_have_same_payload(frame, expected_frame) {
            integrity_failures.insert("correction frame does not match its intent".to_string());
            conflicts.insert(stable_id.to_string());
            continue;
        }
        match receipts.get(stable_id) {
            Some(existing) if !correction::correction_frames_have_same_payload(existing, frame) => {
                integrity_failures.insert("conflicting durable correction frame".to_string());
                conflicts.insert(stable_id.to_string());
            }
            Some(_) => {}
            None => {
                receipts.insert(stable_id.to_string(), frame.clone());
            }
        }
    }

    let integrity_failure = (!integrity_failures.is_empty()).then(|| {
        format!(
            "one or more durable corrections were not applied: {}",
            integrity_failures
                .into_iter()
                .collect::<Vec<_>>()
                .join("; ")
        )
    });
    (
        correction_intents,
        receipts,
        conflicts,
        typed_ids,
        integrity_failure,
    )
}

fn reconstruct_processed_correction_ids(
    rollout_items: &[RolloutItem],
    typed_correction_ids: &HashSet<String>,
    correction_receipts: &BTreeMap<String, ResponseItem>,
    correction_receipt_conflicts: &HashSet<String>,
) -> HashSet<String> {
    rollout_items
        .iter()
        .filter_map(|item| {
            let RolloutItem::EventMsg(EventMsg::RawResponseItem(event)) = item else {
                return None;
            };
            event.persisted_corrections_sampled()
        })
        .flat_map(|sampled| sampled.correction_ids.iter())
        .filter_map(|correction_id| Uuid::parse_str(correction_id).ok())
        .map(correction::correction_frame_stable_id)
        .filter(|stable_id| {
            typed_correction_ids.contains(stable_id)
                && correction_receipts.contains_key(stable_id)
                && !correction_receipt_conflicts.contains(stable_id)
        })
        .collect()
}

fn reconstruct_correction_auto_start_suppressed(rollout_items: &[RolloutItem]) -> bool {
    let mut suppressed = false;
    for item in rollout_items {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnAborted(aborted))
                if aborted.reason == codex_protocol::protocol::TurnAbortReason::Interrupted =>
            {
                suppressed = true;
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_)) => {
                suppressed = false;
            }
            RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_)) => {
                suppressed = false;
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if matches!(&event.item, TurnItem::EnteredReviewMode(_)) =>
            {
                suppressed = false;
            }
            _ => {}
        }
    }
    suppressed
}

pub(super) fn reconstruct_surviving_client_user_message_ids(
    rollout_items: &[RolloutItem],
) -> HashSet<String> {
    #[derive(Default)]
    struct UserBoundary {
        client_id: Option<String>,
        paired: bool,
    }

    fn pair_client_id(boundaries: &mut [UserBoundary], client_id: Option<&str>) {
        let Some(boundary) = boundaries.last_mut() else {
            return;
        };
        if boundary.paired {
            // Mixed retained data can contain both history projections for the same immediately
            // preceding boundary. The active history mode normally persists exactly one.
            return;
        }
        boundary.client_id = client_id.map(str::to_string);
        boundary.paired = true;
    }

    let mut boundaries = Vec::<UserBoundary>::new();
    for item in rollout_items {
        match item {
            RolloutItem::ResponseItem(response_item) if is_user_turn_boundary(response_item) => {
                boundaries.push(UserBoundary::default());
            }
            RolloutItem::InterAgentCommunication(_) => boundaries.push(UserBoundary {
                client_id: None,
                paired: true,
            }),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                if let codex_protocol::items::TurnItem::UserMessage(user_message) = &event.item {
                    pair_client_id(&mut boundaries, user_message.client_id.as_deref());
                }
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(user_message)) => {
                pair_client_id(&mut boundaries, user_message.client_id.as_deref());
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let drop_count = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                boundaries.truncate(boundaries.len().saturating_sub(drop_count));
            }
            RolloutItem::Compacted(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::ResponseItem(_) => {}
        }
    }

    boundaries
        .into_iter()
        .filter_map(|boundary| boundary.client_id)
        .collect()
}

pub(super) fn deduplicate_correction_frames(
    history: Vec<ResponseItem>,
    conflicts: &mut HashSet<String>,
) -> Vec<ResponseItem> {
    let mut first_by_id = BTreeMap::<String, ResponseItem>::new();
    history
        .into_iter()
        .filter(|frame| {
            let Some(stable_id) = super::correction::correction_frame_id(frame) else {
                return true;
            };
            match first_by_id.get(stable_id) {
                Some(existing)
                    if !super::correction::correction_frames_have_same_payload(existing, frame) =>
                {
                    tracing::error!(
                        correction_id = stable_id,
                        "conflicting correction duplicate omitted from model history"
                    );
                    conflicts.insert(stable_id.to_string());
                    false
                }
                Some(_) => false,
                None => {
                    first_by_id.insert(stable_id.to_string(), frame.clone());
                    true
                }
            }
        })
        .collect()
}

impl Session {
    pub(super) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> RolloutReconstruction {
        // Replay metadata should already match the shape of the future lazy reverse loader, even
        // while history materialization still uses an eager bridge. Scan newest-to-oldest,
        // stopping once a surviving replacement-history checkpoint and the required resume metadata
        // are both known; then replay only the buffered surviving tail forward to preserve exact
        // history semantics.
        let has_legacy_compaction_without_window_number =
            rollout_items.iter().any(|item| {
                matches!(item, RolloutItem::Compacted(compacted) if compacted.window_number.is_none())
            });
        let initial_window = if has_legacy_compaction_without_window_number {
            None
        } else {
            rollout_items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(session_meta) => session_meta
                    .meta
                    .context_window
                    .as_ref()
                    .and_then(reconstructed_window_from_session_context_window),
                _ => None,
            })
        };
        let mut base_replacement_history: Option<&[ResponseItem]> = None;
        let mut previous_turn_settings = None;
        let mut reference_context_item = TurnReferenceContextItem::NeverSet;
        let mut world_state_replay = Vec::new();
        let mut window = None;
        let surviving_client_user_message_ids =
            reconstruct_surviving_client_user_message_ids(rollout_items);
        let (
            correction_intents,
            correction_receipts,
            mut correction_receipt_conflicts,
            typed_correction_ids,
            correction_integrity_failure,
        ) = reconstruct_correction_state(rollout_items, &surviving_client_user_message_ids);
        let mut processed_correction_ids = reconstruct_processed_correction_ids(
            rollout_items,
            &typed_correction_ids,
            &correction_receipts,
            &correction_receipt_conflicts,
        );
        let correction_auto_start_suppressed =
            reconstruct_correction_auto_start_suppressed(rollout_items);
        // Rollback is "drop the newest N user turns". While scanning in reverse, that becomes
        // "skip the next N user-turn segments we finalize".
        let mut pending_rollback_turns = 0usize;
        // Borrowed suffix of rollout items newer than the newest surviving replacement-history
        // checkpoint. If no such checkpoint exists, this remains the full rollout.
        let mut rollout_suffix = rollout_items;
        // Reverse replay accumulates rollout items into the newest in-progress turn segment until
        // we hit its matching `TurnStarted`, at which point the segment can be finalized.
        let mut active_segment: Option<ActiveReplaySegment<'_>> = None;

        for (index, item) in rollout_items.iter().enumerate().rev() {
            match item {
                RolloutItem::Compacted(compacted) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                    if active_segment.window.is_none()
                        && let Some(window_number) = compacted.window_number
                    {
                        active_segment.window = Some(ReconstructedWindow {
                            number: window_number,
                            first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
                            previous_id: compacted
                                .previous_window_id
                                .as_deref()
                                .and_then(parse_uuid_v7),
                            id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
                        });
                    }
                    // Looking backward, compaction clears any older baseline unless a newer
                    // `TurnContextItem` in this same segment has already re-established it.
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                    }
                    if active_segment.base_replacement_history.is_none()
                        && let Some(replacement_history) = &compacted.replacement_history
                    {
                        active_segment.base_replacement_history = Some(replacement_history);
                        rollout_suffix = &rollout_items[index + 1..];
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    pending_rollback_turns = pending_rollback_turns
                        .saturating_add(usize::try_from(rollback.num_turns).unwrap_or(usize::MAX));
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // Reverse replay often sees `TurnComplete` before any turn-scoped metadata.
                    // Capture the turn id early so later `TurnContext` / abort items can match it.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = Some(event.turn_id.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                    if let Some(active_segment) = active_segment.as_mut() {
                        if active_segment.turn_id.is_none()
                            && let Some(turn_id) = &event.turn_id
                        {
                            active_segment.turn_id = Some(turn_id.clone());
                        }
                    } else if let Some(turn_id) = &event.turn_id {
                        active_segment = Some(ActiveReplaySegment {
                            turn_id: Some(turn_id.clone()),
                            ..Default::default()
                        });
                    }
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::TurnContext(ctx) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    // `TurnContextItem` can attach metadata to an existing segment, but only a
                    // real `UserMessage` event should make the segment count as a user turn.
                    if active_segment.turn_id.is_none() {
                        active_segment.turn_id = ctx.turn_id.clone();
                    }
                    if turn_ids_are_compatible(
                        active_segment.turn_id.as_deref(),
                        ctx.turn_id.as_deref(),
                    ) {
                        active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                            model: ctx.model.clone(),
                            comp_hash: ctx.comp_hash.clone(),
                            realtime_active: ctx.realtime_active,
                        });
                        if matches!(
                            active_segment.reference_context_item,
                            TurnReferenceContextItem::NeverSet
                        ) {
                            active_segment.reference_context_item =
                                TurnReferenceContextItem::Latest(Box::new(ctx.clone()));
                        }
                    }
                }
                RolloutItem::WorldState(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.world_state_replay.push(item);
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                    // `TurnStarted` is the oldest boundary of the active reverse segment.
                    if active_segment.as_ref().is_some_and(|active_segment| {
                        turn_ids_are_compatible(
                            active_segment.turn_id.as_deref(),
                            Some(event.turn_id.as_str()),
                        )
                    }) && let Some(active_segment) = active_segment.take()
                    {
                        finalize_active_segment(
                            active_segment,
                            &mut base_replacement_history,
                            &mut previous_turn_settings,
                            &mut reference_context_item,
                            &mut world_state_replay,
                            &mut window,
                            &mut pending_rollback_turns,
                        );
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn |= is_user_turn_boundary(response_item);
                }
                RolloutItem::InterAgentCommunication(_) => {
                    let active_segment =
                        active_segment.get_or_insert_with(ActiveReplaySegment::default);
                    active_segment.counts_as_user_turn = true;
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
            }

            if base_replacement_history.is_some()
                && previous_turn_settings.is_some()
                && !matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
            {
                // At this point we have both eager resume metadata values and the replacement-
                // history base for the surviving tail, so older rollout items cannot affect this
                // result.
                break;
            }
        }

        if let Some(active_segment) = active_segment.take() {
            finalize_active_segment(
                active_segment,
                &mut base_replacement_history,
                &mut previous_turn_settings,
                &mut reference_context_item,
                &mut world_state_replay,
                &mut window,
                &mut pending_rollback_turns,
            );
        }

        let fallback_window_number = u64::try_from(
            rollout_items
                .iter()
                .filter(|item| matches!(item, RolloutItem::Compacted(_)))
                .count(),
        )
        .unwrap_or(u64::MAX);

        let mut history = ContextManager::new();
        let mut saw_legacy_compaction_without_replacement_history = false;
        if let Some(base_replacement_history) = base_replacement_history {
            history.replace(base_replacement_history.to_vec());
        }
        // Materialize exact history semantics from the replay-derived suffix. The eventual lazy
        // design should keep this same replay shape, but drive it from a resumable reverse source
        // instead of an eagerly loaded `&[RolloutItem]`.
        for item in rollout_suffix {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(
                        std::iter::once(&response_item),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunicationMetadata { .. } => {}
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement_history) = &compacted.replacement_history {
                        // This should actually never happen, because the reverse loop above (to build rollout_suffix)
                        // should stop before any compaction that has Some replacement_history
                        history.replace(replacement_history.clone());
                    } else {
                        saw_legacy_compaction_without_replacement_history = true;
                        // Legacy rollouts without `replacement_history` should rebuild the
                        // historical TurnContext at the correct insertion point from persisted
                        // `TurnContextItem`s. These are rare enough that we currently just clear
                        // `reference_context_item`, reinject canonical context at the end of the
                        // resumed conversation, and accept the temporary out-of-distribution
                        // prompt shape.
                        // TODO(ccunningham): if we drop support for None replacement_history compaction items,
                        // we can get rid of this second loop entirely and just build `history` directly in the first loop.
                        let user_messages = compact::collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    let anchored_corrections = history
                        .raw_items()
                        .iter()
                        .filter(|frame| {
                            let Some(stable_id) = correction::correction_frame_id(frame) else {
                                return false;
                            };
                            typed_correction_ids.contains(stable_id)
                                && !correction_receipt_conflicts.contains(stable_id)
                                && correction_receipts.get(stable_id).is_some_and(|receipt| {
                                    correction::correction_frames_have_same_payload(receipt, frame)
                                })
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    history.drop_last_n_user_turns(rollback.num_turns);
                    // Typed correction frames are anchored to their explicit target rather than
                    // to whichever later instruction boundary happened to be current when they
                    // were sampled. Re-record any eligible frame that positional rollback removed
                    // at the rollback cut, before later rollout items are replayed.
                    history.record_items(
                        anchored_corrections.iter(),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::SessionMeta(_) => {}
            }
        }

        let reference_context_item = match reference_context_item {
            TurnReferenceContextItem::NeverSet | TurnReferenceContextItem::Cleared => None,
            TurnReferenceContextItem::Latest(turn_reference_context_item) => {
                Some(*turn_reference_context_item)
            }
        };
        let reference_context_item = if saw_legacy_compaction_without_replacement_history {
            None
        } else {
            reference_context_item
        };

        // Segments and their contents were collected newest-first; replay the surviving records
        // chronologically so compaction resets and merge patches have their original meaning.
        world_state_replay.reverse();
        let mut world_state_baseline: Option<WorldStateSnapshot> = None;
        for item in world_state_replay {
            match item {
                RolloutItem::Compacted(_) => world_state_baseline = None,
                RolloutItem::WorldState(world_state) if world_state.full => {
                    world_state_baseline = match serde_json::from_value(world_state.state.clone()) {
                        Ok(snapshot) => Some(snapshot),
                        Err(err) => {
                            tracing::warn!(%err, "failed to restore world-state snapshot");
                            None
                        }
                    };
                }
                RolloutItem::WorldState(world_state) => {
                    let Some(baseline) = world_state_baseline.as_mut() else {
                        tracing::warn!("ignored world-state patch without a full snapshot");
                        continue;
                    };
                    if let Err(err) = baseline.apply_merge_patch(&world_state.state) {
                        tracing::warn!(%err, "failed to apply world-state patch");
                        world_state_baseline = None;
                    }
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::TurnContext(_)
                | RolloutItem::EventMsg(_) => {
                    unreachable!("only world-state replay items are collected")
                }
            }
        }

        let window = window.or(initial_window).unwrap_or(ReconstructedWindow {
            number: fallback_window_number,
            first_id: None,
            previous_id: None,
            id: None,
        });
        let conflicts_before_history = correction_receipt_conflicts.clone();
        let mut history = deduplicate_correction_frames(
            history.into_raw_items(),
            &mut correction_receipt_conflicts,
        );
        let found_history_conflict = correction_receipt_conflicts
            .iter()
            .any(|stable_id| !conflicts_before_history.contains(stable_id));
        let correction_integrity_failure = if found_history_conflict {
            Some(match correction_integrity_failure {
                Some(existing) => {
                    format!("{existing}; conflicting reconstructed correction frame")
                }
                None => "one or more durable corrections were not applied: conflicting \
                         reconstructed correction frame"
                    .to_string(),
            })
        } else {
            correction_integrity_failure
        };
        processed_correction_ids
            .retain(|stable_id| !correction_receipt_conflicts.contains(stable_id));
        history.retain(|item| {
            let Some(stable_id) = correction::correction_frame_id(item) else {
                return true;
            };
            if !typed_correction_ids.contains(stable_id) {
                return true;
            }
            !correction_receipt_conflicts.contains(stable_id)
                && correction_receipts.get(stable_id).is_some_and(|receipt| {
                    correction::correction_frames_have_same_payload(receipt, item)
                })
        });
        RolloutReconstruction {
            history,
            surviving_client_user_message_ids,
            correction_intents,
            correction_receipts,
            correction_receipt_conflicts,
            correction_integrity_failure,
            processed_correction_ids,
            correction_auto_start_suppressed,
            previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number: window.number,
            first_window_id: window.first_id,
            previous_window_id: window.previous_id,
            window_id: window.id,
        }
    }
}

fn parse_uuid_v7(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
}

fn reconstructed_window_from_session_context_window(
    context_window: &SessionContextWindow,
) -> Option<ReconstructedWindow> {
    let id = parse_uuid_v7(&context_window.window_id)?;
    Some(ReconstructedWindow {
        number: 0,
        first_id: Some(id),
        previous_id: None,
        id: Some(id),
    })
}
