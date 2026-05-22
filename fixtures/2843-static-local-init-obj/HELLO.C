int next_id(void) {
  static int counter = 100;
  counter = counter + 1;
  return counter;
}
