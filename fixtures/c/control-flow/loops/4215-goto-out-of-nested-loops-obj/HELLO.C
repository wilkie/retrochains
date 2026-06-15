int main(void) {
  int i;
  int j;
  int found;
  found = 0;
  for (i = 0; i < 4; i = i + 1) {
    for (j = 0; j < 4; j = j + 1) {
      if (i + j == 5) {
        found = i * 10 + j;
        goto out;
      }
    }
  }
out:
  return found;
}
