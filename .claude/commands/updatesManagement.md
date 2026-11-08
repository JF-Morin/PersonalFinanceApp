# Goal
The goal of this prompt is redefine and fix the updating issues

# Requirements
- Rework the updating mechanism on the app
- When a transaction is added. updated or removed from the database, the server need to notify all the connected users:
    - The concerned tables need to refresh their data with the database data
    - All other related ui elements need to update to:
        - "A completer" button with the users incomplete transactions
        - The home page graph and "Solde..." section
    - Importing a file (with a successful parsing) has the same behavior
- Basically, a form sends a post (or other modifying request) with a change to the database, every app need to refresh

# Instructions
- You can use SSE or any technique to achieve this goal
- You can modify the backend and frontend code
- You can adjust database settings/config to achieve this too
- You can make suggestion too
