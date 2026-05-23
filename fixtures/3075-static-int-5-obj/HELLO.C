int get(void) {
  static int counter = 5;
  counter = counter + 1;
  return counter;
}
