# App Requirements

## Overview

This application's purpuse is to parse pdf files into transaction and store on a databse with the help of a web server.

## Deployment

This application will be deployed on a local personal NAS server running unRaid.
This application will be containerized and run on unRaid.
I would also like to be portable to docker/podman on another machine (ex: proxmox running a VM)
There's no need for this application to be available on unRaid's store.
The container config should be modifyable through the config files (compose)

## Tech Stack

- **Backend:**
- The backend should ne written in rust.
- Axum shall be used as the router.
- Tokio shall be used for async and threads (if needed)

- **Frontend:**
- The frontend should use HTMX, AlpineJS and Tailwinds
- Keep it light and responsive 

- **Database:**
- SQLite shall be used for the database

- **Storage**
- All ingested PDF files shall be store on a SMB share
- The SMB share location should be defined in the app settins
- Here's the directory structure:
    -/PersonalFinanceApp
        -/Statements
            -/Bank_1
            -/Bank_2
        -/Images

- **PDF parsing:**
- There should be 1 parser per bank per type of file
- These parser we be developed with the help of Claude-code and added to the code when needed.

- **Containerization:**
- The container shall contain all the stack and database, so one container for everything except the SMB share and the store pdf files

## Data Model

This is a rough schema of the objects/database:

- Users: id[primary key], name[string], last_name[string, nullable], bank_id [foreign key, nullable]
- Banks: id[primary key], name[string], logo[string, nullable]
- Imported_Files: id[primary key], filename[string], bank_id[foreign key], source_file[string], imported_at[date], row_count[int], importer_id[forein key]
- Transactions: id[primary key], date[date], transaction_date[date], operation[string], amount[decimal (positive/negative)], currency_id[foreign key], category_id[foreign key, nullable], paid_by[foreign key (user id)], other_share[decimal (0-100%), nullable], status_id[foreign key],from_file_id[foreign key], notes[string, nullable]
- Transaction_Status: id[primary key], name[string]
- Categories: id[primary key], name[string]
- Currency: id[primary key], name[string], symbole[string]

**Note: for a transaction to be "complete", the Category and "other_share" shall be populated

## PDF Ingestion & Parsing

- The PDF files will be loaded from the webapp ui with a drop zone or a upload button in a designated page
- Once uploaded, the file type and bank source are recognized and the proper parser is used to create the transaction
- The file is also stored in a SMB share

### Parser Interface

- In the parser/upload page, the user provides one or more files to parse.
- Once parsed,  a list of transactions is presented in a summerized fashon (concise table) if parsed Successfully
- If the file is not recognized by the default parser, check if for the proper parser in the app. If there's one, use it
- If no parser are able to do the job, let the user know with a popup message/modal message

### Default parsers

#### BNP Paribas
Here's the format of the table found in the PDf file of BNP Paribas' statement and what they should correspond to:
column 1: date => date
column 2: Nature des operations => Operation
column 3: Valeur => transaction_date
column 4: Debit => amount (negative decimal number)
column 5: Credit => amount (positive decimal number)

**note: some lines are not to be parsed into transactions because they represent the account amount ("Solde crediteur au ...")
**note: some lines are not to be parsed into transactions because they represent the total amount of the operations ("Total des operations") => this could be use for the arsing validation for credit/debit

#### Credit Mutuel
Here's the format of the table found in the PDf file of Credit Mutuel's statement and what they should correspond to:
column 1: date => transaction_date
column 2: Date Valeur => (not useful)
column 3: Operation => Operation
column 4: Debit Euros => amount (negative decimal number)
column 5: Credit Euros => amount (positive decimal number)

**note: in the 3rd column, operation, a 4-digit sequence can be present. It represent the "Date". If not present, use the "Date Valeur" (2nd column) for the date
**note: Sometimes, the 1st and 2nd values of a row are missing. This means that the Operation (3rd column) text comes from the previous row
**note: some lines are not to be parsed into transactions because they represent the account amount ("Solde crediteur au ...")

## Web UI

### Pages / Views

/ or /home - landing page of the app (menu + summerized info)
    - this page should show who owes what to whom with only the transactions with a status of "complete" and not "pending"
    - the sum owed should take into account the currency
    - manual transaction could also be added
/dashboard - page with more info/stats about the app and database
/settings - a page with all the settings (some setting TBD)
    there should be a setting for:
        -the theme
        -default currency
        -(other settings TBD)
/imported-files - a page with the list of all imported files and their metadata (info)
/transactions - a page with all the transactions in a table (filters)
/incomple-transactions - a page where the user can complete their own transaction's: "pending" transactions

### Designs

- The app shall use Tailwind for styling/theming
- A light and dark theme shall be the 2 starting available theme: configurable in the /settings page
- The "Material design" from google should be used
- The UI should be simple, but effective
- The use of font-awesome icons is strongly recommended

### Review & Confirmation Flow

Steps for using the app:
i. if the user is not identified, he/she shall do so
ii. the user can go to the /settings page if desired

1. A user uploads a PDF file 
2. After being parsed, the list of info is display and transactions are added to the DB as well as the file to the file storage
3. The user can then go to the /incomplete-transaction page to complete their transactions

## Out of Scope

Claude-code shall not add unnecessary features or code.
If Claude-code has suggestions for improvement, it can resume them so that I can add those later

<!-- Explicitly list what this app will NOT do, to keep scope clear -->
