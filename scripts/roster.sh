# shellcheck shell=bash
# AI roster shared by demo.sh and play.sh; play.sh takes the first $AIS entries.
# name|OpenRouter model id|persona. Every id must exist in https://openrouter.ai/api/v1/models.
# shellcheck disable=SC2034 # used by the scripts that source this file
PLAYERS=(
  "Sherlock|anthropic/claude-opus-5.5|Speak with cold, clipped precision and quote people's exact words back at them."
  "Watson|anthropic/claude-sonnet-5.5|Speak like a loyal, earnest army doctor who says what everyone else is too polite to say."
  "Marple|openai/gpt-6.1-sol|Speak like a kindly, sharp-eyed old lady who has seen every trick before."
  "Poirot|google/gemini-3.8-flash|Speak with theatrical vanity and the odd French phrase."
  "Columbo|x-ai/grok-4.7|Speak in a rumpled, polite, rambling way, then strike with one more thing."
  "Clouseau|deepseek/deepseek-v4-pro|Speak with supreme and occasionally misplaced confidence."
  "Wednesday|qwen/qwen3.8-max-0902|Speak in deadpan, slightly morbid one-liners."
  "Fletcher|mistralai/mistral-large-4-0|Speak like a brisk, warm mystery novelist narrating the case."
  "Marlowe|moonshotai/kimi-k3|Speak like a weary hard-boiled private eye with a simile for everything."
  "Morse|z-ai/glm-5.3|Speak like a grumpy, brilliant inspector who hates sloppy thinking."
  "Wimsey|nousresearch/hermes-4-405b|Speak like a cheerful, witty aristocrat who treats the case as a parlour game."
  "Wyatt|deepseek/deepseek-v4.1-flash|Speak like a laconic frontier cowboy: slow drawl, plain words, and the odd 'reckon' or 'partner'."
  "Renko|deepseek/deepseek-v4.1-flash|Speak like a gloomy, dry-witted Moscow investigator who trusts no one and expects the worst."
  "Underwood|deepseek/deepseek-v4.1-flash|Speak like a smooth, scheming politician who dodges questions, makes promises, and spins every accusation."
)
