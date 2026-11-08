# Goal

The goal of this prompt is to fix the issues with the updating of the app.

## UI
- Brind back the "Annuler" and "Enregistrer" buttons for the settings page, and all the forms: edit, new transaction, "A completer" form, login, create new user
- For shares in the edit, new and to complete transaction forms, if the total for the shares is above the maximum, revert all the share textbox to the previous value and dont save anything.
- Side fix: in the home page, for the 4 elements at the top, make the 4th one ("imported") the same layout as the others: One icon + 2 stacked texts. Also, for all of them, make the icons a bit bigger


## Navigation

## Behavior
- For every table in the ui of the application, make all of them either refresh with a page refresh of with a SSE.
- Update the home page home page on SSE too.
- SSE should occur when any of the tables in the database is modified

## Misc.

## Additional info
- Please use the logs to helps yourself with the debugging
- Don't try to run docker compose up --build, I'll run it myself. Just tell me when you're done
