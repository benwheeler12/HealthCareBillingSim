Introduction
We’re excited to see how you tackle this exercise to get a sense of your:
A. Ability to learn (slope)
B. Grit (slope-derivative)
C. Computer science knowledge (y intercept)
D. Product sense (is this even the right graph?)
E. Communication skills (do you make pretty plotlines? I’m pushing this metaphor too far).
We will review your solution in a live follow-up conversation. Our objective is to have an insightful
technical discussion around your code, your reasoning, and the design choices you’ve made.
There is no specific deadline; reach out to schedule your follow up when you are ready. We expect you
to use all resources available to you, just like if we were to work together. You can and should use AI
codegen as you deem appropriate. A submission with deliverables that clearly go above and beyond
will score highly on B) and likely the other components of our rubric. A game-winner will do so with a
faster turnaround time than you would have considered possible 12 or 24 months ago.
Project
The goal of this project is to assess your ability to design, implement, test, and reason about a system
that simulates the healthcare billing life-cycle. You will write a program that implements a real-time
simulation facilitating the interactions between a billing service, a healthcare clearinghouse, and
insurance payers.
Along the way, please be prepared to discuss broader software engineering topics such as
concurrency/parallelism, performance tradeoffs, system design, testing, monitoring, usability, and
real-world constraints. Your solution should be eﬃcient, complete, and correct. It should use all the
resources available to the machine as best as it can.
Why this matters
Our mission is to ensure that patients receive the insurance dollars they deserve and that providers get
paid for the care they deliver. This requires a critical set of capabilities — from understanding what the
patient’s insurance will cover, to providing cost estimates to the patient upfront, to fighting wrongful
claim denials. Each depends on fast, accurate, and reliable ‘negotiations’ with healthcare
clearinghouses and insurance payers. The better our infrastructure for executing these negotiations,
the greater impact we can make on patients and providers alike. Onward.
Your Task
You may use any programming language you would like and you may use any resources available to you,
including AI 🧠. The goal is to demonstrate your ability to write complete, correct, maintainable, and
performant software systems. Bonus points for writing memory-safe, parallelizeable, and concurrent
software that can fully utilize the underlying hardware.
1. Ingest the claims from the input file with a configurable rate (e.g. 1 claim per second)
●
●
Your program must accept a single command line argument that points to the input file. The file
will be plaintext and will contain exactly one json object per line. The file may be arbitrarily long
or short.
The PayerClaim JSON Schema below describes a well-formed claim - though we can’t promise
every claim you receive will be formatted correctly.
2. Submit the claim to the payer via a simulated clearinghouse:
●
After ingesting the claim, the billing company will send the claim to the payer via a healthcare
clearinghouse.
●
You must implement this clearinghouse yourself. Its job is to receive claim documents from the
biller then forward them to the relevant insurance payer as determined by the payer_id.
3. Forward the claim to the payer and back to the claim submitter (billing company/practice)
●
The clearinghouse is responsible for receiving the claims from the practitioner and forwarding
them to the correct payer.
●
Like real clearinghouses, you must bake in a little unreliability: occasionally make it fail to deliver
documents. The biller must be resilient to these failures - we fight tooth and nail for our
customers. It’s not abnormal to send a claim and not hear back the first time.
4. Adjudicate the insurance payer decision
●
Payers receive the payer claim documents from clearinghouses, determine how much they will
pay, assign patient responsibility as needed, and respond with a payer remittance advice
document to the clearinghouse. In the real-world, they use a document called EDI 835.
●
You must define the payer remittance advice object (you can research EDI 835 for inspiration). It
must be capable of assigning a payer_paid_amount, coinsurance_amount, copay_amount,
deductible_amount, and not_allowed_amount for each service line in each claim.
●
You can decide how you want to arbitrate the payment values.
●
The time that it takes for each payer to respond should be randomly generated from a
distribution bounded by the payer's min_response_time_secs and max_response_time_secs
parameters.
5. Forward the remittance response back to the original submitter
●
The clearinghouse is responsible for receiving the remittance from the payer and forwarding it
to the original submitter.
6. Interpret the remittance data and turn it into insight for the practice owner. You should think through
what a practice owner would find important. The format of the insight is up to you but be able to defend
your reasoning.
●
As a starting point, consider an AR (Accounts Receivable) aging report by payer. We recommend
you spend a few minutes with ChatGPT/Claude to learn how AR aging works and why providers
care about it.
●
What else does a practice owner need to know? How might we best present that information to
them?
7. Testing: Your program should be testable and you should demonstrate how. We won’t be looking for
100% coverage, just good fundamentals.
8. Monitoring: Your program should emit what it needs to such that we can diagnose and understand
its behavior at runtime
9. Shutdown: After all claims have been fully processed and their final stats have been serialized to the
terminal, the program should gracefully shutdown.
Deliverables
1. ZIP File via email
a. Source code
b. README
c. Tests
2. Live discussion: In addition to the coding portion, be prepared to:
a. Walk us through your system and the decisions behind it.
b. We will introduce a new requirement and ask you to build on it with us. Be ready to
iterate on your work.
Closing
1. 2. 3. 4. 5. This prompt is intentionally open-ended: Make your own assumptions, design it like you would
a production system, and document your key decisions, assumptions, and tradeoffs. It’s your job
to communicate how and why you did what you did.
Be creative: Feel free to go beyond the requirements.
We value readability & simplicity: Well-abstracted, logical, documented code is a requirement.
Have compassion for the users of your code (aka us).
We’ll discuss: We’re not just looking at the final result but also at how you communicate and
reason about design, tradeoffs, performance, and maintainability.
Keep the infra simple & stick to a single machine: Write software that runs in a single process.
Avoid complicated external tools/systems e.g. the kafka’s of the world. No need for network
calls. You don’t need to use docker unless you want to, etc. Build the thing yourself!
Good luck. We look forward to reviewing your submission and discussing it with you.
- Brace Health Engineering
PayerClaim JSON Schema
{
"$schema": "http://json-schema.org/draft-07/schema#"
,
"type": "object"
,
"required": ["claim_id"
,
"rendering_provider"
,
"place_of_service_code"
,
"service_lines"],
"insurance"
,
"properties": {
"claim_id": {
"type": "string"
"patient"
,
},
"place_of_service_code": {
"type": "integer"
},
"insurance": {
"type": "object"
,
"required": ["payer_id"
,
"patient_member_id"],
"properties": {
"payer_id": {
"type": "string"
,
"enum": ["medicare"
,
"united_health_group"
,
"anthem"],
"description": "Must be one of the major US insurance payers"
},
"patient_member_id": {
"type": "string"
}
"organization"
,
}
},
"patient": {
"type": "object"
,
"required": ["first_name"
,
"properties": {
"first_name": {
"type": "string"
},
"last_name": {
"type": "string"
},
"email": {
"type": "string"
},
"last_name"
,
"gender"
,
"dob"],
"gender": {
"type": "string"
,
"pattern": "^(m|f)$"
},
"dob": {
"type": "string"
,
"format": "date"
},
"address": {
"type": "object"
,
"properties": {
"street": {
"type": "string"
},
"city": {
"type": "string"
},
"state": {
"type": "string"
},
"zip": {
"type": "string"
},
"country": {
"type": "string"
}
}
}
}
},
"organization": {
"type": "object"
,
"required": ["name"],
"properties": {
"name": {
"type": "string"
},
"billing_npi": {
"type": "string"
},
"ein": {
"type": "string"
},
"contact": {
"type": "object"
,
"properties": {
"first_name": {
"type": "string"
},
"last_name": {
"type": "string"
},
"phone_number": {
"type": "string"
}
}
},
"address": {
"type": "object"
,
"properties": {
"street": {
"type": "string"
},
"city": {
"type": "string"
},
"state": {
"type": "string"
},
"zip": {
"type": "string"
},
"country": {
"type": "string"
}
}
}
}
},
"rendering_provider": {
"type": "object"
,
"required": ["first_name"
,
"properties": {
"first_name": {
"type": "string"
},
"last_name": {
"type": "string"
},
"npi": {
"last_name"
,
"npi"],
"type": "string"
,
"pattern": "^\\d{10}$"
}
}
},
"service_lines": {
"type": "array"
,
"items": {
"type": "object"
,
"required": ["service_line_id"
,
"unit_charge_amount"],
"properties": {
"service_line_id": {
"type": "string"
"procedure_code"
,
"units"
,
"details"
,
"unit_charge_currency"
,
},
"procedure_code": {
"type": "string"
},
"modifiers": {
"type": "array"
,
"items": {
"type": "string"
}
},
"units": {
"type": "integer"
,
"minimum": 1
},
"details": {
"type": "string"
},
"unit_charge_currency": {
"type": "string"
},
"unit_charge_amount": {
"type": "number"
,
"minimum": 0
},
}
"do_not_bill": {
"type": "boolean"
}
}
}
}
}