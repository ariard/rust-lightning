Contributing to Rust-Lightning
==============================

The Rust-Lightning project operates an open contributor model where anyone is
wlecome to contribute towards development in the form of peer review, documentation,
testing and patches.

Anyone is invited to contribute without regard to technical experience, "expertise", OSS
experience age or other social discriminant. Though developing cryptocurrencies demand a
high-level bar of rigor, adversial thinking, thorough testing and risk-minimization.
Any bug may cost real-money and so impact severely people lifes. That's said we're deeply
welcoming of people contributing for the first time on an open source project or pick up
Rust on the ground. Don't be shy, you'll learn.

Communications Channels
-----------------------

Communication about Rust-Lightning happens on #rust-bitcoin IRC.

Discussion about code base improvements happens in GitHub issues and on pull
requests.

Contribution Workflow
--------------------

The codebase is maintained using the "contributor workflow" where everyone
without exception contributes patch proposals using "pull requests". This
facilitates social contribution, easy testing and peer review.

To contribute a patch, the worflow is a as follows:

  1. Fork Repository
  1. Create topic branch
  1. Commit patches


In general commits should be atomic and diffs should be easy to read.
For this reason do not mix ant formatting fixes or code moves with
actual code changes.

When adding a new feature, like implementing a BOLT spec object, thought
must be given to the long term technical debt. Every new features should
be covered by functional tests.

When refactoring, structure your PR to make it easy to review and don't
hesitant to split in multiple small, focused PRs.

Peer review
-----------

Anyone may participate in peer review which is expressed by comments in the pull
request. Typically reviewers will review the code for obvious errors, as well as
test out the patch set and opine on the technical merits of the patch. PR should
be reviewed first on the conceptual level before focusing on code style or grammar
fixes.

Architecture
------------

XXX: (here of in readme ?)


Security
--------

Security is the primary focus of Rust-Lightning, disclosure of security vulnerabilites
helps prevent user loss of funds. If you think vulnerability is on the spec level,
please inform other Lightning implementations diligently.

XXX: (what process ?)

Testing
-------

Deeply tied with the security aspect, Rust-Lightning developers take testing
really seriouslt. Due to the modular nature of the project writing new functional
tests is easy and well-coverage of the codebase is a long-term goal.

Fuzzing is heavily-encouraged, you will find all related fuzzing stuff under `fuzz/`

Mutation testing is work-in-progess, any contrubition there would be warmly welcomed.

Going further
-------------

You may be interested by Jon Atack guide on [How to review Bitcoin Core PRs](https://github.com/jonatack/bitcoin-development/blob/master/how-to-review-bitcoin-core-prs.md) and [How to make
Bitcoin Core PRs] (https://github.com/jonatack/bitcoin-development/blob/master/how-to-make-bitcoin-core-prs.md). Modulo projects context and diffference of maturity there is a lot to
stick to.

Overall, have fun :)
