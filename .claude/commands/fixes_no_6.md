# Observations

This is a file with the observations of the app after the 6th compilation

## UI
- Add a delete button beside pdf files in the /imported_file page imported by the user

## Navigation

## Behavior
- For the delete button beside the pdf files, when press, ask the user if he/she also want to delete the associated transactions along with the files:
    - if yes, delete the transactions derived from the pdf file and give a summary of the number of transactions deleted. Then,  delete the pdf file in the storage and in the db table
    - if no, just delete the file in the storage and in the db table

## Additional info
- Please use the logs to helps yourself with the debugging
