int main(void) {
  unsigned int mask;
  int i;
  int count;
  mask = 0xA5;
  count = 0;
  for (i = 0; i < 8; i = i + 1) {
    if (mask & (1 << i)) count = count + 1;
  }
  return count;
}
