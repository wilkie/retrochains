enum level { LOW = 1, MID = 2, HIGH = 3 };
int score(enum level x) {
  int n;
  n = 0;
  switch (x) {
    case HIGH: n = n + 100;
    case MID:  n = n + 10;
    case LOW:  n = n + 1;
              break;
    default:   n = -1;
  }
  return n;
}
int main(void) {
  enum level v = MID;
  if (v == MID)
    return score(v) + score(HIGH);
  return 0;
}
