static int counter;
int tick(void) {
  counter = counter + 1;
  return counter;
}
