# Observations

This prompt's purpose is to restructure the ui and templates

## UI
- In the transaction table (/transactions) for each item, add a pdf file logo for parsed transactions and a "file-pen" for manual transactions


## Navigation
- When attempting to reach a path, always check if the user is logged in.
    - If not logged in, navigate to /login and the login template
    - If logged in, go to the desired path
- All the pages have the same layout except the login:
    - Left side: menu panel:
        - Top: logo + name => navigates to "/" (home)
        - Just under, User => no navigation
        - Menu:
            - Home => navigates to "/"
            - Transactions => navigates to "/transactions"
            - Imports => navigates to "/imported-files"
            - Settings => navigates to "/settings"
        - Logout => logs the user out => navigates to /login
    - Right side (main section):
        - Top bar: contains the page's name ("Accueil", "Transactions", "Fichiers importes", "Parametres")
        - Main section: contains the diferent content depending on the pages (keep whats already coded)
- Modals can be displayed from anywhere on the app
    - Modals can display dialog message boxes or forms
    - The forms for the carousel and edit are build the same way:
        - All have the same ui
        - When in carousel mode, the bottom buttons are: Previous and Next or Terminated
            - When one of those is clicked and one of the properties of the transactions has been modified, the UI send a patch/put request to the server
        - When in edit mode, the bottom button are: Cancel and Save
            - Only the save button send a patch/put request

## Behavior
- Home widgets:
    - All of them are updated only when a SSE is fired for the transaction db table or the imported_files
- Transaction widgets:
    - All of them are updated only when a SSE is fired for the transaction db table
- Imported files widgets:
    - All of them are updated only when a SSE is fired for the imported_files db table
- Settings widgets:
    - All of the are updated only when a SSE is fired for the user table in the db aand any toher related tables
    

## Misc.
- Use htmx as much as possible, espacially for all GET, DELETE, POST, PUT, PATCH and all other http requests (and also SSE)
- Use Alpinejs (Alpinejs.dev) for all ui only interactions
- Remove as much js as possible, or replace it with Alpinejs.

## Additional info
- Please use the logs to helps yourself with the debugging
- Don't try to run docker compose up --build, I'll run it myself. Just tell me when you're done
