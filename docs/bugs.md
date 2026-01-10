# Bugs

## 1) Transcript lines vanish during new response render

- **Summary**: While a new assistant response is streaming, previously visible transcript lines (not top/bottom) disappear from the scrollback.
- **Observed flow**:
  - User sees full prior assistant output.
  - User types a new message and submits.
  - As streaming begins, arbitrary earlier lines vanish, even before much new output appears.
- **Impact**: Prior context disappears mid‑session; user can’t reference earlier content; breaks multi‑threaded workflow.
- **Frequency**: Often; “pretty frequent”.
- **Notes**: Happens even when user is focused on current output; also visible after scrolling down.
- **Extra detail**: Often coincides with the live “Explored” tool output section updating (e.g., list of `List/Read/Search` steps).

## 2) History navigation / edit selection jumps backward, removes previewed chunk

- **Summary**: Using double‑escape to navigate message history and edit prior content sometimes jumps past the intended step and removes previewed transcript chunk.
- **Observed flow**:
  - User presses `Esc Esc` to enter history/edit mode.
  - Navigates further back (e.g., `Esc` twice more).
  - Presses Enter to load the selected step.
  - Result: lands at an earlier step than intended; previewed chunk disappears.
- **Impact**: Can’t reliably edit earlier turns; transcript integrity compromised.
- **Frequency**: Intermittent.
- **Notes**: Possibly related to transcript rendering issue above; also noticed after resume in some sessions.
