# Observations

This is a file with the observations of the app after the 8th compilation

## UI
- Remove the /to_complete (a completer) page entirely
- In the Transactions page, add a button to show a carousel of the not completed transactions (layout and ui phone friendly):
    - The carousel shows the non-completed transaction 1 by one in a modal (keep the phone layout in mind)
    - Each card for the carousel has the missing elements to complete it (part, category & notes[optional])
    - At the bottom of the card, a Previous and Next buttons help to cycle between the non-completed transaction cards
    - Edits in the cards are automatocally saved
    - If either bottom button is pressed, check if the transaction if completed. If so, remove that transaction card from the carousel. If not, keep it, but navigate to the previous/next.
    - Leave room around the cards so that the user can click around the card to hide/close the entire carousel. You can also add a subtil "x" (close) button.
    - Add a "Export to CSV" button for the transaction table/list. This pops up a dialog box to select a file destination.
    - Add button to filter transactions by year
    - For the textbox, filter the transactions in the table after each key press
    - Remove the "Filtrer" button
    - When an option is selected in the category dropdown in the transactions table filter row, automatically filter the rows.
    - Suggest a better font for the application. Something that is easier to read.
    - Make the font size bigger
    - The "Add manual transaction" should be in the /transactions page
    - Merge the Dashboard and Home page into one and remove the redundant metrics


## Navigation

## Behavior
- When transaction is completed, please recalculate the amount owed to each other:
    - Calculate the total paid by User 1 for User 2: amount * part. Include both the credits and debits. To the same for the other way around.
    - Total owed to one, is Total_User_1 - Total_User_2 if Total 1 > Total 2. Then User 2 owes the difference. Or vive versa.

## Additional info
- Please use the logs to helps yourself with the debugging
