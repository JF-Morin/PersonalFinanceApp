# Goals
- The goal of this command is to describe the parsers of this PersonalFinanceApp 

## General Workflow
- The user can import a PDF bank statement from the his/her bank
- When a file is imported, here's the workflow of the backend:
    1. With a hash function (value), the app must check if the same file was imported before by looking in the database table for imported files
        1.1 if the file was already imported, ask the user if he/she wants to re-parse the file and add the missing transactions (via popup message)
            1.1.1 If the user refuses, discard the import requests
                1.1.1.1 Don't save the request file 
                1.1.1.2 Don't create new transactions.
                1.1.1.3 Notify the user that this particular file won't be saved
            1.1.2 If the user accepts:
                1.1.2.1 From the PDF file data in the storage, check to see if the statement is from the user's bank => see Checking the bank of the statement
                1.1.2.2 Parse every transactions in the table and store them in a local list (local variable in the rust code, not in the database nor a local file) => see parsing requirements for more details
                1.1.2.3 Compare each item of this local list against the database list (date, operation, paid_by, amount)
                    1.1.2.3.1 If the transaction exists, ignore the item in the local list
                    1.1.2.3.2 If the transaction doesn't exist, add the transaction to the database
                1.1.2.4 When all the items are compared, give a summary of the missing transactions that were just added
        1.2 If the file was not already imported:
            1.2.1 From the PDF file data, check to see if the statement is from the user's bank => see Checking the bank of the statement
                1.2.1.1 If not the proper bank, notify the user that this action is not possible for him/her
                1.2.1.2 If no bank parser was detect, notify the user there's an error: no parser detected for this file
                1.2.1.3 Continue with the parsing of the file
            1.2.2 Parse the file and store the transaction items in a local list in the rust code (not on the databse, nor on a local file)
                1.2.2.1 If a transaction cannot be parsed or is not parsed properly, notify the user, don't save any transaction to the database and reject (don't store) the pdf file
                1.2.2.2 If everything parses correctly, continue with the validation
            1.2.3 Validate the parsing:
                1.2.3.1 Calculate the sum of all the transactions (credit = positive amount, debit = negative amount)
                1.2.3.2 Calculate the difference between the  "Solde..." at the begining of the end of the month and the one at the start of the month
                    1.2.3.2.1 If they different, reject the parsing and import action: don't add any transaction to the database and don't store the file in the storage. Notify the user of the error.
                    1.2.3.2.2 If everything checks out, continue with the workflow
            1.2.4 If success, add the transactions to the database, add the file info to the database table (imported_files), and add the file to the storage folder
            1.2.5 Notify the user of the resul: failure or success (with the number of transactions)



## Checking the bank of the statement
- To check the bank of the statement, here's a list of things to check in order:
    1. File name containing the bank's name
    2. File data containing the bank's name
    3. File data containing the first name of last name of the user
    4. File data containing a transactions table in the same format as the user's bank table format (same amount of columns, same columns name, etc)
    5. Trying to parse the file data with the user's bank parser
    6. Trying to parse the file data with the other user's bank parser
- If the bank is detected by the end of step 5, then the parsing can start
- If on step 6. the app detects the other user's bank parser, stop the parsing and notify the user that he/she cannont proceed
- If no bank parser was detected, notify the user that the file cannont be parsed (error)

## Parsing
This is the section for the parsers.
For both parsers, the transaction amount is determined by the location of the value;
    - If the value is in the "Credit" column, the amount of the transaction is positive
    - If the value is in the "Debit" column, the amount of the transaction is negative
For both parsers, be aware that for the month of december and/or january, there might be some transaction of the previous/next year

### BNP Paribas
  Here's the format of the table found in the PDf file of BNP Paribas' statement and what they should correspond to:
  column 1: date => (not useful)
  column 2: Nature des operations => Operation
  column 3: Valeur => transaction_date
  column 4: Debit => amount (negative decimal number)
  column 5: Credit => amount (positive decimal number)
  
  **note: some lines are not to be parsed into transactions because they represent the account amount ("Solde crediteur au ...") at the begining or the end of the month
  **note: some lines are not to be parsed into transactions because they represent the total amount of the operations ("Total des operations") => this could be use for the parsing validation for credit/debit
  **note: some transaction operation descriptions span 2 rows (empty other columns for that row)
  **note: in the "nature des operations" column, sometimes there will be a date in the DDMMYY format that represents the "date". If not present, use the value in the first column as the "date"
  
### Credit Mutuel
  Here's the format of the table found in the PDf file of Credit Mutuel's statement and what they should correspond to:
  column 1: date => transaction_date
  column 2: Date Valeur => (not useful)
  column 3: Operation => Operation
  column 4: Debit Euros => amount (negative decimal number)
  column 5: Credit Euros => amount (positive decimal number)
  
  **note: in the 3rd column, operation, a 4-digit sequence can be present. It represent the "Date". If not present, use the "Date Valeur" (2nd column) for the date
  **note: Sometimes, the 1st and 2nd values of a row are missing. This means that the Operation (3rd column) text comes from the previous row
  **note: some lines are not to be parsed into transactions because they represent the account amount ("Solde crediteur au ...")


## Reparsing request from imported files page
- In the imported files page, add a button for each file in the table:
    - This button prompts (popup message) the user the reparse the file to recreate missing (or deleted by mistake) transactions
    - If he/she refuse, then close the popup and cancel the action
    - If the user agrees, then do the procedure starting from step 1.1.2 and don't import the file
