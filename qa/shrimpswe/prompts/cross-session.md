We keep getting inconsistent error handling across this codebase and it's biting us in
production — some functions throw raw strings, some throw Error, some return null.

Define a DEDICATED error type for this project — invent a name for it, put it in
`src/errors.ts`, and write down the convention somewhere a future contributor will find it.
Then apply it to `src/handlers/h1.ts`.

Use your judgement on the name and the shape. Just make it explicit and consistent.
