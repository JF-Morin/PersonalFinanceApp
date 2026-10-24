# Goals
- The goal here is to develop a strategy to deploy this app on a unraid docker container

# Requirements
- Be able to pull the app from github
- Have the ENV vars displayed in the unraid container config page
- Make sure that every settings, env vars and other config are presented to me so i can set them manually (if needed, for example the first deployment)
- Before a push, offer a way to test the new modifications on my local machine
- Before a new app update is applied to the unraid docker container, create a backup of the database in /data/db-backup in case something goes wrong. This needs to happen on the server, not the local machine

# Constraints
- Make sure that no confidential files are deployed to github
- Recheck before every push that no confidential file are to be publish on my github account
- Make sure that the gitignore file is up to date with these constraints

# Strategy
- 1. Claude-code will explain in great details how to achieve the goal
- 2. Help me to go through a step by step guide to achieve the goal, espacially the CI/CD portion

# Observations (to add later)
