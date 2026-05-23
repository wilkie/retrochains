long get(void) {
  static long counter = 0L;
  counter = counter + 1L;
  return counter;
}
