int counter = 0;
void inc(void) { counter++; }
void inc2(void) { counter += 2; }
int main(void) {
  inc(); inc2(); inc();
  return counter;
}
