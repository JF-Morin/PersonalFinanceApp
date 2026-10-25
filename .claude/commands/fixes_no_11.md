# Observations

This is a file with the observations of the app after the 10th compilation

## UI
- Add a create User in the login screen


## Navigation
- If not connected, display a "Cannot access" or "Not authorized" message with a button to return to /login

## Behavior
- Make sure to have a router guard for every path in the api: I was able to bypass the /login by typing /settings.
- Make sure the auth and access is checked for every path with the current connected user.
- Create a system to track logged in users

## Misc.
- Remove unnecessary files : i think the project still has files related to Dashboard
- Create an icon, picture or svg (use th most appropriate) for my unraid container icon. Put the file in a relevant folder. 

## Additional info
- Please use the logs to helps yourself with the debugging
- Don't try to run docker compose up --build, I'll run it myself. Just tell me when you're done
