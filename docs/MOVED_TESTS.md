# Moved tests

Every test of the three signal-web crates (baseline commit `4324d53`) and where it lives now.
Counts are from `cargo test -p <crate> -- --list` in both repositories (unit, integration and doc tests).

| Crate | signal-web | vault | Parked |
|---|---|---|---|
| container -> vault-container | 147 | 147 | 0 |
| vault-proto -> vault-proto | 23 | 23 | 0 |
| vault-server -> vault-server | 37 | 7 | 30 |

Test binaries: `unittests src/...` are the unit tests inside the crate, `tests/*.rs` are integration test files, `doc` are doc tests. Doc-test paths are shown as in signal-web (`container/src/...` is `vault-container/src/...` here).

## Parked

The tests of `vault-server/tests/session.rs` and `vault-server/tests/vault_dev.rs` start an in-process
signal-web `relay` server, which does not exist in this repository yet. They are copied verbatim (apart from the
`container` -> `vault_container` rename and the MIT header) to `vault-server/tests-pending/`, which cargo
does not build. Task 2 (`vault-mailbox`) retargets them at `vault-relay` and moves them back to
`vault-server/tests/`; their rows say `parked` until then.

## Tests

| Test | Binary | Was (signal-web crate) | Now |
|---|---|---|---|
| `backend::s3::sigv4::tests::amz_date_formats` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::sigv4::tests::sigv4_matches_aws_vectors` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::sigv4::tests::uri_encoding` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::debug_redacts_secrets` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::dotted_bucket_needs_path_style` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::extra_root_ca_parsing` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::list_nesting_depth_is_capped` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::parses_list_pages` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::plain_http_only_for_loopback_or_opt_in` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::prefix_segments_must_be_plain` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::rejects_bad_config` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::s3::tests::url_styles` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::auth_header_is_sensitive_basic` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::base_url_is_normalised_and_checked` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::credentials_are_checked` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::empty_multistatus_is_an_empty_list` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::extra_root_ca_must_parse` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::parses_prefixes_encodings_and_absolute_hrefs` | `unittests src/lib.rs` | `container` | `vault-container` |
| `backend::webdav::tests::rejects_doctype_wrong_root_and_garbage` | `unittests src/lib.rs` | `container` | `vault-container` |
| `engine::tests::header_roundtrip_and_hostile_input` | `unittests src/lib.rs` | `container` | `vault-container` |
| `engine::tests::high_water_checks` | `unittests src/lib.rs` | `container` | `vault-container` |
| `engine::tests::large_batches_split_into_bounded_segments` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::debug_redacts_secrets` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::digit_aliases_map_to_letters` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::known_answer_wrap_blob_and_aad` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::known_answers` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::parse_ignores_any_whitespace` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::recovery_key_typo_detected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::recovery_roundtrip_display_and_qr` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::subkeys_are_distinct_and_deterministic` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::wrap_unwrap_roundtrip_and_format` | `unittests src/lib.rs` | `container` | `vault-container` |
| `keys::tests::wrong_recovery_key_fails_unwrap` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::auth_tag_binds_every_field` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::duplicate_segment_ids_rejected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::manifest_roundtrip_padded` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::nonzero_padding_and_garbage_rejected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::open_rejects_inconsistent_chain_link` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::size_limits` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::successor_requires_seq_plus_and_prev_hash` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::tampered_objects_rejected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `manifest::tests::wrapped_crk_capped` | `unittests src/lib.rs` | `container` | `vault-container` |
| `secrets::tests::seal_roundtrip_and_binding` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::authenticated_but_malformed_streams_rejected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::hostile_record_bodies_rejected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::last_chunk_flag_prevents_truncation` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::padding_hides_exact_size` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::record_contents_zeroize_on_drop` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::segment_roundtrip` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::size_limits` | `unittests src/lib.rs` | `container` | `vault-container` |
| `segment::tests::tampered_objects_rejected` | `unittests src/lib.rs` | `container` | `vault-container` |
| `commit_conflict_keeps_pending` | `tests/engine.rs` | `container` | `vault-container` |
| `compaction_of_empty_container` | `tests/engine.rs` | `container` | `vault-container` |
| `compaction_preserves_state_and_drops_old_segments` | `tests/engine.rs` | `container` | `vault-container` |
| `concurrent_manifest_commit_conflicts` | `tests/engine.rs` | `container` | `vault-container` |
| `crash_before_manifest_commit_recovers_last_state` | `tests/engine.rs` | `container` | `vault-container` |
| `crash_before_manifest_commit_recovers_last_state_local` | `tests/engine.rs` | `container` | `vault-container` |
| `create_on_non_empty_backend_conflicts` | `tests/engine.rs` | `container` | `vault-container` |
| `crud_and_scan_roundtrip` | `tests/engine.rs` | `container` | `vault-container` |
| `delete_then_reopen` | `tests/engine.rs` | `container` | `vault-container` |
| `gc_refuses_when_manifest_moved` | `tests/engine.rs` | `container` | `vault-container` |
| `manifest_forged_with_only_manifest_key_is_rejected` | `tests/engine.rs` | `container` | `vault-container` |
| `missing_manifest_is_rollback_or_incomplete_create` | `tests/engine.rs` | `container` | `vault-container` |
| `missing_segment_is_an_error` | `tests/engine.rs` | `container` | `vault-container` |
| `open_with_wrong_keys_fails` | `tests/engine.rs` | `container` | `vault-container` |
| `orphan_segments_garbage_collected` | `tests/engine.rs` | `container` | `vault-container` |
| `oversized_record_rejected_without_side_effects` | `tests/engine.rs` | `container` | `vault-container` |
| `reload_rejects_rollback` | `tests/engine.rs` | `container` | `vault-container` |
| `reload_with_missing_manifest_is_rollback` | `tests/engine.rs` | `container` | `vault-container` |
| `reopen_replays_to_same_state` | `tests/engine.rs` | `container` | `vault-container` |
| `reopen_with_high_water_rejects_rollback` | `tests/engine.rs` | `container` | `vault-container` |
| `scan_prefix_ordering` | `tests/engine.rs` | `container` | `vault-container` |
| `segment_id_collision_retries_once` | `tests/engine.rs` | `container` | `vault-container` |
| `stats_track_live_and_dead_bytes` | `tests/engine.rs` | `container` | `vault-container` |
| `successor_not_chained_to_high_water_is_rollback` | `tests/engine.rs` | `container` | `vault-container` |
| `concurrent_commit_across_instances` | `tests/local.rs` | `container` | `vault-container` |
| `concurrent_manifest_commit_conflicts` | `tests/local.rs` | `container` | `vault-container` |
| `conformance_put_if_absent` | `tests/local.rs` | `container` | `vault-container` |
| `local_atomic_write_survives_crash` | `tests/local.rs` | `container` | `vault-container` |
| `local_etag_is_sha256_hex` | `tests/local.rs` | `container` | `vault-container` |
| `local_open_cleans_probe_temp_files` | `tests/local.rs` | `container` | `vault-container` |
| `local_open_creates_seg_and_probe_dirs` | `tests/local.rs` | `container` | `vault-container` |
| `clones_share_state` | `tests/memory.rs` | `container` | `vault-container` |
| `concurrent_manifest_commit_conflicts` | `tests/memory.rs` | `container` | `vault-container` |
| `conformance_put_if_absent` | `tests/memory.rs` | `container` | `vault-container` |
| `adopt_diverged_overwrites_after_confirmation` | `tests/replicate.rs` | `container` | `vault-container` |
| `adopting_an_older_manifest_reports_rollback` | `tests/replicate.rs` | `container` | `vault-container` |
| `crash_after_manifest_write_is_recognised` | `tests/replicate.rs` | `container` | `vault-container` |
| `deletes_recheck_the_replica_manifest` | `tests/replicate.rs` | `container` | `vault-container` |
| `diverged_replica_not_overwritten` | `tests/replicate.rs` | `container` | `vault-container` |
| `diverged_replica_without_header_gets_no_header` | `tests/replicate.rs` | `container` | `vault-container` |
| `one_replica_failure_does_not_block_others` | `tests/replicate.rs` | `container` | `vault-container` |
| `primary_header_must_match_manifest` | `tests/replicate.rs` | `container` | `vault-container` |
| `replica_failure_retries_and_reports` | `tests/replicate.rs` | `container` | `vault-container` |
| `replica_rollback_is_flagged_and_repaired` | `tests/replicate.rs` | `container` | `vault-container` |
| `replica_set_keeps_state_of_absent_replicas` | `tests/replicate.rs` | `container` | `vault-container` |
| `replicate_is_idempotent` | `tests/replicate.rs` | `container` | `vault-container` |
| `replication_converges` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_never_wipes_a_replica_or_unverified_leftovers` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_picks_highest_and_flags_rollback` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_refuses_below_known_high_water` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_rejects_broken_chain` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_rejects_manifest_forged_with_only_manifest_key` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_resumes_after_header_written_without_manifest` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_retries_after_interrupted_target` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_same_seq_fork_needs_user_choice` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_then_newer_unreachable_replica_is_not_overwritten` | `tests/replicate.rs` | `container` | `vault-container` |
| `restore_with_max_seq_mark_does_not_overflow` | `tests/replicate.rs` | `container` | `vault-container` |
| `concurrent_manifest_commit_conflicts` | `tests/s3.rs` | `container` | `vault-container` |
| `conditional_request_conflict_409_is_conflict` | `tests/s3.rs` | `container` | `vault-container` |
| `conformance_against_mock_s3` | `tests/s3.rs` | `container` | `vault-container` |
| `list_paginates_and_is_scoped_to_prefix` | `tests/s3.rs` | `container` | `vault-container` |
| `list_rejects_stuck_alternating_and_endless_tokens` | `tests/s3.rs` | `container` | `vault-container` |
| `missing_bucket_is_an_error_not_absence` | `tests/s3.rs` | `container` | `vault-container` |
| `mock_rejects_unsigned_requests` | `tests/s3.rs` | `container` | `vault-container` |
| `not_implemented_maps_to_unsupported` | `tests/s3.rs` | `container` | `vault-container` |
| `oversized_bodies_are_refused` | `tests/s3.rs` | `container` | `vault-container` |
| `probe_accepts_conforming_provider` | `tests/s3.rs` | `container` | `vault-container` |
| `probe_rejects_provider_without_conditional_writes` | `tests/s3.rs` | `container` | `vault-container` |
| `server_errors_are_io_errors_without_retry` | `tests/s3.rs` | `container` | `vault-container` |
| `arc_dyn_backend_runs_on_spawned_tasks` | `tests/send.rs` | `container` | `vault-container` |
| `cli_create_put_replicate_restore_roundtrip` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `create_refuses_nonempty_dir` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `diverged_replica_exits_2_and_adopt_needs_confirmation` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `failed_restore_leaves_target_untouched` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `restore_probes_source_conditional_writes` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `restore_refuses_nonempty_dir` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `secrets_file_is_private_encrypted_and_corruption_is_reported` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `webdav_url_with_credentials_and_empty_passphrase_are_rejected` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `wrong_passphrase_fails` | `tests/vaultctl.rs` | `container` | `vault-container` |
| `collections_are_created_lazily_and_recreated_if_removed` | `tests/webdav.rs` | `container` | `vault-container` |
| `concurrent_manifest_commit_conflicts` | `tests/webdav.rs` | `container` | `vault-container` |
| `conformance_against_mock_webdav` | `tests/webdav.rs` | `container` | `vault-container` |
| `debug_output_hides_credentials` | `tests/webdav.rs` | `container` | `vault-container` |
| `delete_missing_is_ok_and_names_are_validated` | `tests/webdav.rs` | `container` | `vault-container` |
| `follow_up_that_shows_other_content_is_an_error_not_an_etag` | `tests/webdav.rs` | `container` | `vault-container` |
| `hostile_propfind_responses_are_rejected` | `tests/webdav.rs` | `container` | `vault-container` |
| `list_decodes_hrefs_and_ignores_foreign_entries` | `tests/webdav.rs` | `container` | `vault-container` |
| `missing_etag_on_put_is_fetched_with_follow_up_get` | `tests/webdav.rs` | `container` | `vault-container` |
| `mkcol_405_means_already_exists` | `tests/webdav.rs` | `container` | `vault-container` |
| `oversized_get_is_refused_and_missing_is_none` | `tests/webdav.rs` | `container` | `vault-container` |
| `probe_accepts_conforming_server` | `tests/webdav.rs` | `container` | `vault-container` |
| `probe_rejects_server_without_etag_preconditions` | `tests/webdav.rs` | `container` | `vault-container` |
| `probe_rejects_weak_etags` | `tests/webdav.rs` | `container` | `vault-container` |
| `redirects_are_not_followed` | `tests/webdav.rs` | `container` | `vault-container` |
| `requests_carry_basic_auth_and_wrong_password_fails` | `tests/webdav.rs` | `container` | `vault-container` |
| `container/src/engine.rs - engine::Container<B>::create (line 271)` | `doc` | `container` | `vault-container` |
| `container/src/engine.rs - engine::Container<B>::create (line 278)` | `doc` | `container` | `vault-container` |
| `frame::tests::chunking_roundtrip_and_cap` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `frame::tests::reassembler_rejects_interleaved_or_gapped` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `frame::tests::reassembler_requires_increasing_msg_id` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::field_caps_on_decode` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::messages_roundtrip` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::ping_wire_tag_is_pinned` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::qr_roundtrip_and_rejects_bad_prefix` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::rejects_trailing_bytes` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::relay_url_strictness` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `msg::tests::relay_url_validation` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::debug_is_redacted` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::forged_frame_poisons_both_directions` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::handshake_and_roundtrip` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::initiator_rejects_foreign_secret` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::mitm_relay_detected` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::oversize_frame_rejected` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::oversize_plaintext_rejected_and_poisons` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::pair_keys_are_fresh_and_consistent` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::reordered_frame_aborts` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::replayed_frame_aborts` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::sas_format_is_zero_padded_and_pinned` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::sas_matches_both_sides` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `noise::tests::zeroizing_resolver_interoperates_with_default` | `unittests src/lib.rs` | `vault-proto` | `vault-proto` |
| `approver::tests::end_of_input_denies` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `approver::tests::late_answer_does_not_approve_next_prompt` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `approver::tests::parses_answers` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `approver::tests::prompt_shows_relay_host` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `approver::tests::stale_and_mismatched_lines_are_ignored` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `transport::tests::extra_ca_must_parse` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `transport::tests::mailbox_urls` | `unittests src/lib.rs` | `vault-server` | `vault-server` |
| `approval_timeout_closes` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `approval_timeout_starts_when_prompt_is_shown` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `approvals_are_serialized_and_bound_to_the_shown_sas` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `at_most_two_sessions_await_approval` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `commit_failure_reported_as_error` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `compaction_never_folds_another_sessions_pending_writes` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `compaction_runs_after_session_when_threshold_crossed` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `compaction_runs_when_dead_bytes_dominate` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `crud_commit_roundtrip` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `denied_approval_closes` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `handshake_with_wrong_browser_key_fails` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `idle_timeout_ends_session` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `large_value_chunked_roundtrip` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `lock_ends_session` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `lock_ends_session_awaiting_approval` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `malformed_message_closes_session` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `no_compaction_below_threshold` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `panic_in_request_still_cleans_up` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `ping_is_answered_in_every_state` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `requests_before_approval_refused` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `second_session_terminates_first` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `timeout_ends_session` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `uncommitted_writes_discarded_when_session_ends` | `tests/session.rs` | `vault-server` | parked: `vault-server/tests-pending/session.rs` |
| `approve_persists_put_and_commit` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
| `bad_offer_and_unreachable_relay_exit_nonzero` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
| `ctrl_c_saves_high_water` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
| `deny_and_eof_end_session_without_persisting` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
| `mailbox_taken_warns_not_to_confirm_the_browser` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
| `many_commits_are_compacted_at_exit_and_high_water_saved` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
| `wrong_passphrase_exits_nonzero_without_dialing` | `tests/vault_dev.rs` | `vault-server` | parked: `vault-server/tests-pending/vault_dev.rs` |
