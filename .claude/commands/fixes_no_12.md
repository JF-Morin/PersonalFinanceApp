# Observations

This is a file with the observations of the app after the 12th compilation

## UI

### Home Page
- For the "Solde..." section:
    - For each user, have these rows:
        - Total debited (debit: sum of negative amounts) [foreground = red]
        - Total credided (credit: sum of positive amounts) [foreground = green]
        - Total debited for the other user (sum of (negative amount * other's share)) [foreground = red]
        - Total credited for the other user (sum of (positive amount * other's share)) [foreground = green]
        - Total for the other (tot. credit for other[pos. amount] + tot. debit for other [neg. amount])
            - If total is negative => the other owes user the total amount
            - If total is positive => the user owes the other the total amount
    - Last row:
        - Compare the total owed to/by the other, and the difference is the total owes to user or other

### Transactions Page
- For the "montant" column:
    - If a transaction as a "foreign_currency_amount" populated, show this formated text instead:
        - (-/+) (amount + symbole) [foreign amount + foreign symbol]
    - Keep the - or + if the transaction is a debit/credit operation
- In the table, the columns "Operation" and "Note" shall use the most space. All the other should only use the space they need horizontaly

### Settings page
- Make the table wider to accomodate the longer texts

### Popup forms
- Make the popup forms wider to accomodate the longer texts


## Behavior
- When a transaction is added, if the currency is not the same as the default currency, save the amount into another column: foreign_currency_amount
- Then, fill the "amount" column with the converted amount to the default currency : (foreign currency amount) * (convertion rate) = (default currency amount).
- The conversion rate is must be the one officially given for the given day of the transaction: check online for it
- When a "date" for a transaction with a foreign currency is changed, update the amount
- When a "foreign currency amount" for a transaction with a foreign currency is changed, update the amount
- When both have changed, update the amount too

## Misc.

## Additional info
- Please use the logs to helps yourself with the debugging
- Don't try to run docker compose up --build, I'll run it myself. Just tell me when you're done
