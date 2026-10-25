# Goals
- The goal of this prompt is to reorganize the shares of all participants

## Requirements
- In the database "transactions" table
    - Change the "paid_by" field name by "owner_id"
    - Remove the "other_share" field
    - Create a new table "transaction_shares"
        - user_id: id to the user object (users table), not null
        - transaction_id: id to a transaction in the "transactions" table, not null
        - share: number greater than 0 (never 0), and less or equal to 100%, not null or 0

## UI
- Add an element/control in the edit form (and duplicate it in the carousel) to create/edit a participant's share
- In the transactions page, for each row, in the "Part" column/cell, create a list of pill-shaped indicators representing the participants of the transaction with their share  
    - Left side of the pill shape indicator: circle with the initials of the participant
    - On the right side, their share as a percentage
- If no participants where added, just keep the yellow "(N/A)"
- In the home page, for the "solde..." section, reorganize the rows to represent the behavior listed below
- In the home page, for the "Solde..." section, the last row should have a list of what owes what to whom

## Behavior
- When a new transaction is added to the table, no new row is created in the "transaction_shares" table
- In the transaction edit form, or in the carousel, the user can add his share (0 < share <= 100%), or someone else's. 
    - Of course, the user can only add an existing user of the database
- A transaction cannot be "completed" without the sum of the shares being equal to 100%
- For the home page, now the calculation becomes a bit different
    - Each amount is calculated from the transaction_shares table joined with the "amount" in the transactions table
    - For each user:
        - Total (debit) = every entry in the "transaction_shares" where id = user * share(%) * transaction amount (where value is negative)
        - Total (credit) = every entry in the "transaction_shares" where id = user * share(%) * transaction amount (where value is positive)
        - For each other user:
            - Total (debit) = every entry in the "transaction_shares" where id = other_user * share(%) * transaction amount (where value is negative)
            - Total (credit) = every entry in the "transaction_shares" where id = other_user * share(%) * transaction amount (where value is positive)
            - Total for other user: total credit + total  debit


