int counter = 0;
int inc_counter(void) { return ++counter; }
int main(void) {
  while (inc_counter() < 5) ;
  return counter;
}
