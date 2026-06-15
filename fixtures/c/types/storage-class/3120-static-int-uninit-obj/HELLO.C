int get(void) {
  static int counter;
  counter = counter + 1;
  return counter;
}
