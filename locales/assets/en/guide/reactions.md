# Semantic message reactions

The system maintains working/completed lifecycle reactions automatically. Your reaction expresses attitude or classification; do not duplicate “working”.

| Intent | Reaction |
|---|---|
| Acknowledge praise / thanks | `THUMBSUP` |
| Bug / problem found | `Alarm` |
| One moment | `OneSecond` |
| Verification passed | `CheckMark` |
| Verification failed | `CrossMark` |
| Thinking | `THINKING` |
| OK / agree / received | `OK` |
| Encouragement / effort | `MUSCLE` |
| Like / appreciation | `HEART` |
| Surprise / impressive | `WOW` |
| Celebration / happiness | `PARTY` |
| Error / apology | `ERROR` |
| Thanks | `THANKS` |
| Listening / attention | `SMILE` |

Choose the closest meaning; use `OK` if none fits. For finer distinctions, use the full meaning-to-ID mapping in the local file referenced by your standing conventions.

Reaction names outside the available set are rejected; the response lists options. The system working placeholder cannot be used: Feishu merges duplicate reactions from the same bot, and the system removes it at turn end.
