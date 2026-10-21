# Observations

This is a file with the observations of the app after the 2nd compilation

## UI
- In the import pdf page, make it so that the user can import 1 or more files.
- The user can only parse the pdf from its bank: so if the app doesn't recognize/parse 1 or more imported pdf files, reject them and dont save them in the database
- If 1 or more files are imported, create a list (still in the same import page) with these buttons: Analyze All, Delete all, Analyze (for each file), Delete (for each file)

## Navigation

## Behavior
- In the import page, after adding some files the use can do these actions with the previously described buttons:
    - Analyze all: parse all the file (can be a parallel process)
    - Delete all: remove all the files (not saved to the DB)
    - Analyze: analyze that one file
    - Delete: delete that one file

## Additional info
- Please use the logs to helps yourself with the debugging
