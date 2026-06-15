int main(void) {
  int i;
  int found = -1;
  for (i = 0; i < 100; i = i + 1) {
    if (i == 42) {
      found = i;
      break;
    }
  }
  return found;
}
